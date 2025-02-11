use jaq_core;
use jaq_interpret::{Ctx, Filter, FilterT, ParseCtx, RcIter, Val};
use jaq_std;
use proxy_wasm::traits::*;
use serde_json::Value as JsonValue;
use std::any::Any;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::config::get_config_value;
use crate::data::{Input, State};
use crate::nodes::{Node, NodeConfig, NodeFactory, PortConfig};
use crate::payload::Payload;

#[derive(Clone)]
pub struct Jq {
    inputs: Vec<String>,
    filter: Filter,
}

impl NodeConfig for Rc<Jq> {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct Errors(Vec<String>);

impl<T: Into<String>> From<T> for Errors {
    fn from(value: T) -> Self {
        Errors(vec![value.into()])
    }
}

impl Errors {
    fn new() -> Self {
        Self(vec![])
    }

    fn push<E>(&mut self, e: E)
    where
        E: Into<String>,
    {
        self.0.push(e.into());
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    #[cfg(test)]
    fn into_inner(self) -> Vec<String> {
        self.0
    }
}

impl From<Errors> for State {
    fn from(val: Errors) -> Self {
        State::Fail(vec![Some(Payload::Error(if val.is_empty() {
            // should be unreachable
            "unknown jq error".to_string()
        } else {
            val.0.join(", ")
        }))])
    }
}

impl Jq {
    fn new(jq: &str, inputs: Vec<String>) -> Result<Self, String> {
        let mut defs = ParseCtx::new(inputs.clone());

        defs.insert_natives(jaq_core::core());
        defs.insert_defs(jaq_std::std());

        if !defs.errs.is_empty() {
            for (err, _) in defs.errs {
                log::error!("jq: input error: {err}");
            }
            return Err("failed parsing filter inputs".to_string());
        }

        let (parsed, errs) = jaq_parse::parse(jq, jaq_parse::main());
        if !errs.is_empty() {
            for err in errs {
                log::error!("filter parse error: {err}");
            }
            return Err("invalid filter".to_string());
        }

        let Some(parsed) = parsed else {
            return Err("parsed filter contains no main handler".to_string());
        };

        // compile the filter in the context of the given definitions
        let filter = defs.compile(parsed);
        if !defs.errs.is_empty() {
            for (err, _) in defs.errs {
                log::error!("filter compile error: {err}");
            }
            return Err("filter compilation failed".to_string());
        }

        Ok(Jq { inputs, filter })
    }

    fn exec(&self, inputs: &[Option<&Payload>]) -> Result<Vec<JsonValue>, Errors> {
        if inputs.len() != self.inputs.len() {
            return Err(Errors::from(format!(
                "invalid number of inputs, expected: {}, got: {}",
                self.inputs.len(),
                inputs.len()
            )));
        }

        let mut errs = Errors::new();

        let vars_iter = self
            .inputs
            .iter()
            .zip(inputs.iter())
            .map(|(name, input)| -> Val {
                match input {
                    Some(input) => match input.to_json() {
                        Ok(value) => value.into(),
                        Err(e) => {
                            errs.push(format!("jq: input error at {name}: {e}"));
                            Val::Null
                        }
                    },
                    None => Val::Null,
                }
            });

        let input_iter = {
            let iter = std::iter::empty::<Result<Val, String>>();
            let iter = Box::new(iter) as Box<dyn Iterator<Item = Result<Val, String>>>;
            RcIter::new(iter)
        };
        let input = Val::Null;

        let ctx = Ctx::new(vars_iter, &input_iter);

        let results: Vec<JsonValue> = self
            .filter
            .run((ctx, input))
            .map(|item| match item {
                Ok(v) => v.into(),
                Err(e) => {
                    errs.push(e.to_string());
                    JsonValue::Null
                }
            })
            .collect();

        if !errs.is_empty() {
            return Err(errs);
        }

        Ok(results)
    }
}

impl Node for Rc<Jq> {
    fn run(&self, _ctx: &dyn HttpContext, input: &Input) -> State {
        match self.exec(input.data) {
            Ok(results) => {
                match results.len() {
                    // empty
                    0 => State::Done(vec![None]),

                    // one or more
                    _ => State::Done(
                        results
                            .into_iter()
                            .map(|item| Some(Payload::Json(item)))
                            .collect(),
                    ),
                }
            }
            Err(errs) => errs.into(),
        }
    }
}

pub struct JqFactory {}

fn sanitize_jq_inputs(inputs: &[String]) -> Vec<String> {
    // TODO: this is a minimal implementation.
    // Ideally we need to validate input names into valid jq variables
    inputs
        .iter()
        .map(|input| input.replace('.', "_").replace('$', ""))
        .collect()
}

impl NodeFactory for JqFactory {
    fn default_input_ports(&self) -> PortConfig {
        PortConfig {
            defaults: None,
            user_defined_ports: true,
        }
    }

    fn default_output_ports(&self) -> PortConfig {
        PortConfig {
            defaults: None,
            user_defined_ports: true,
        }
    }

    fn new_config(
        &self,
        _name: &str,
        inputs: &[String],
        _outputs: &[String],
        bt: &BTreeMap<String, JsonValue>,
    ) -> Result<Box<dyn NodeConfig>, String> {
        let filter = get_config_value(bt, "jq").unwrap_or(".".to_string());
        let inputs = sanitize_jq_inputs(inputs);
        let jq = Jq::new(&filter, inputs)?;

        Ok(Box::new(Rc::new(jq)))
    }

    fn new_node(&self, config: &dyn NodeConfig) -> Box<dyn Node> {
        match config.as_any().downcast_ref::<Rc<Jq>>() {
            Some(jq) => Box::new(jq.clone()),
            None => panic!("incompatible NodeConfig"),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::json;

    #[test]
    fn filter_sanity() {
        let jq = Jq::new("{ a: $a, b: $b }", vec!["a".to_string(), "b".to_string()]);

        let Ok(jq) = jq else {
            panic!("jq error");
        };

        let a = Payload::Json(json!({
            "foo": "bar",
            "arr": [1, 2, 3],
        }));

        let b = Payload::Json(json!("some text"));

        let inputs = vec![Some(&a), Some(&b)];

        let res = jq.exec(inputs.as_slice());

        let Ok(results) = res else {
            panic!("unexpected jq error");
        };

        assert_eq!(
            results,
            vec![json!({
                "a": {
                    "foo": "bar",
                    "arr": [1, 2, 3]
                },
                "b": "some text"
            })]
        );
    }

    #[test]
    fn invalid_filter_text() {
        let jq = Jq::new("nope!", Vec::new());

        let Err(e) = jq else {
            panic!("expected invalid filter to result in an error");
        };

        assert_eq!("invalid filter", e.to_string());
    }

    #[test]
    fn empty_filter() {
        let jq = Jq::new("", vec![]);

        let Err(e) = jq else {
            panic!("expected invalid filter to result in an error");
        };

        assert_eq!("invalid filter", e.to_string());
    }

    #[test]
    fn filter_errors() {
        let jq = Jq::new("error(\"woops\")", vec![]).unwrap();

        let res = jq.exec(&[]);
        let Err(errs) = res else {
            panic!("expected a failure");
        };

        assert_eq!(errs.into_inner(), vec!["woops"]);
    }

    #[test]
    fn invalid_number_of_inputs() {
        let jq = Jq::new("$foo", vec!["foo".to_string()]).unwrap();

        let res = jq.exec(&[]);
        let Err(errs) = res else {
            panic!("expected a failure");
        };

        assert_eq!(
            errs.into_inner(),
            vec!["invalid number of inputs, expected: 1, got: 0"]
        );
    }
}
