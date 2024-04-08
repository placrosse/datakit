use log;
use proxy_wasm::traits::*;
use serde_json::Value;
use std::any::Any;
use std::collections::BTreeMap;
use std::time::Duration;
use url::Url;

use crate::config::get_config_value;
use crate::data;
use crate::data::{Payload, State, State::*};
use crate::nodes::{Node, NodeConfig, NodeFactory};

#[derive(Clone, Debug)]
pub struct CallConfig {
    // FIXME: the optional ones should be Option,
    // but we're not really serializing this for now, just deserializing...

    // node-specific configuration fields:
    url: String,
    method: String,
    timeout: u32,
}

impl NodeConfig for CallConfig {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Clone)]
pub struct Call {
    config: CallConfig,

    token_id: Option<u32>,
}

impl Node for Call {
    fn run(&mut self, ctx: &dyn HttpContext, inputs: Vec<Option<&Payload>>) -> State {
        log::debug!("call: run");

        let body = inputs.first().unwrap_or(&None);
        let headers = inputs.get(1).unwrap_or(&None);

        let call_url = match Url::parse(self.config.url.as_str()) {
            Ok(u) => u,
            Err(err) => {
                log::error!("call: failed parsing URL from 'url' field: {}", err);
                return Done(None);
            }
        };

        let host = match call_url.host_str() {
            Some(h) => h,
            None => {
                log::error!("call: failed getting host from URL");
                return Done(None);
            }
        };

        let mut headers_vec = data::to_pwm_headers(*headers);
        headers_vec.push((":method", self.config.method.as_str()));
        headers_vec.push((":path", call_url.path()));

        let body_slice = data::to_pwm_body(*body);

        let trailers = vec![];
        let timeout = Duration::from_secs(self.config.timeout.into());

        let host_port = match call_url.port() {
            Some(port) => format!("{}:{}", host, port),
            None => host.to_owned(),
        };

        let result = ctx.dispatch_http_call(
            &host_port,
            headers_vec,
            body_slice.as_deref(),
            trailers,
            timeout,
        );

        match result {
            Ok(id) => {
                log::debug!("call: dispatch call id: {:?}", id);
                self.token_id = Some(id);
                Waiting(id)
            }
            Err(status) => {
                log::error!("call: error: {:?}", status);
                Done(None)
            }
        }
    }

    fn resume(&mut self, ctx: &dyn HttpContext, _inputs: Vec<Option<&Payload>>) -> State {
        log::debug!("call: resume");

        let r = if let Some(body) = ctx.get_http_call_response_body(0, usize::MAX) {
            let content_type = ctx.get_http_call_response_header("Content-Type");

            Payload::from_bytes(body, content_type.as_deref())
        } else {
            None
        };

        Done(r)
    }

    fn is_waiting_on(&self, token_id: u32) -> bool {
        self.token_id == Some(token_id)
    }
}

pub struct CallFactory {}

impl NodeFactory for CallFactory {
    fn new_config(
        &self,
        _name: &str,
        _inputs: &[String],
        bt: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn NodeConfig>, String> {
        Ok(Box::new(CallConfig {
            url: get_config_value(bt, "url", String::from("")),
            method: get_config_value(bt, "method", String::from("GET")),
            timeout: get_config_value(bt, "timeout", 60),
        }))
    }

    fn new_node(&self, config: &dyn NodeConfig) -> Box<dyn Node> {
        match config.as_any().downcast_ref::<CallConfig>() {
            Some(cc) => Box::new(Call {
                config: cc.clone(),
                token_id: None,
            }),
            None => panic!("incompatible NodeConfig"),
        }
    }
}
