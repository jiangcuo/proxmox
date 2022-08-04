use std::collections::HashMap;
use std::io::Read;

use anyhow::{format_err, Error};
use http::Response;

use crate::HttpClient;
use crate::HttpOptions;

pub const DEFAULT_USER_AGENT_STRING: &str = "proxmox-sync-http-client/0.1";

#[derive(Default)]
/// Blocking HTTP client for usage with [`HttpClient`].
pub struct Client {
    options: HttpOptions,
}

impl Client {
    pub fn new(options: HttpOptions) -> Self {
        Self { options }
    }

    fn agent(&self) -> Result<ureq::Agent, Error> {
        let mut builder = ureq::AgentBuilder::new();
        if let Some(proxy_config) = &self.options.proxy_config {
            builder = builder.proxy(ureq::Proxy::new(proxy_config.to_proxy_string()?)?);
        }

        Ok(builder.build())
    }

    fn exec_request(
        &self,
        req: ureq::Request,
        body: Option<&[u8]>,
    ) -> Result<Response<Vec<u8>>, Error> {
        let req = req.set(
            "User-Agent",
            self.options
                .user_agent
                .as_deref()
                .unwrap_or(DEFAULT_USER_AGENT_STRING),
        );

        let res = match body {
            Some(body) => req.send_bytes(body),
            None => req.call(),
        }?;

        let mut builder = http::response::Builder::new()
            .status(http::status::StatusCode::from_u16(res.status())?);

        for header in res.headers_names() {
            if let Some(value) = res.header(&header) {
                builder = builder.header(header, value);
            }
        }
        let mut body = Vec::new();
        res.into_reader().read_to_end(&mut body)?;
        builder
            .body(body)
            .map_err(|err| format_err!("Failed to convert HTTP response - {err}"))
    }

    fn convert_body_to_string(res: Response<Vec<u8>>) -> Result<Response<String>, Error> {
        let (parts, body) = res.into_parts();
        let body = String::from_utf8(body)?;
        Ok(Response::from_parts(parts, body))
    }
}

impl HttpClient<String> for Client {
    fn get(
        &self,
        uri: &str,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<String>, Error> {
        let mut req = self.agent()?.get(uri);

        if let Some(extra_headers) = extra_headers {
            for (header, value) in extra_headers {
                req = req.set(header, value);
            }
        }

        self.exec_request(req, None)
            .and_then(Self::convert_body_to_string)
    }

    fn post(
        &self,
        uri: &str,
        body: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<Response<String>, Error> {
        let mut req = self.agent()?.post(uri);
        if let Some(content_type) = content_type {
            req = req.set("Content-Type", content_type);
        }

        self.exec_request(req, body.map(|b| b.as_bytes()))
            .and_then(Self::convert_body_to_string)
    }

    fn request(&self, request: http::Request<String>) -> Result<Response<String>, Error> {
        let mut req = self
            .agent()?
            .request(request.method().as_str(), &request.uri().to_string());
        let orig_headers = request.headers();

        for header in orig_headers.keys() {
            for value in orig_headers.get_all(header) {
                req = req.set(header.as_str(), value.to_str()?);
            }
        }

        self.exec_request(req, Some(request.body().as_bytes()))
            .and_then(Self::convert_body_to_string)
    }
}

impl HttpClient<Vec<u8>> for Client {
    fn get(
        &self,
        uri: &str,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Response<Vec<u8>>, Error> {
        let mut req = self.agent()?.get(uri);

        if let Some(extra_headers) = extra_headers {
            for (header, value) in extra_headers {
                req = req.set(header, value);
            }
        }

        self.exec_request(req, None)
    }

    fn post(
        &self,
        uri: &str,
        body: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<Response<Vec<u8>>, Error> {
        let mut req = self.agent()?.post(uri);
        if let Some(content_type) = content_type {
            req = req.set("Content-Type", content_type);
        }

        self.exec_request(req, body.map(|b| b.as_bytes()))
    }

    fn request(&self, request: http::Request<Vec<u8>>) -> Result<Response<Vec<u8>>, Error> {
        let mut req = self
            .agent()?
            .request(request.method().as_str(), &request.uri().to_string());
        let orig_headers = request.headers();

        for header in orig_headers.keys() {
            for value in orig_headers.get_all(header) {
                req = req.set(header.as_str(), value.to_str()?);
            }
        }

        self.exec_request(req, Some(request.body()))
    }
}
