use crate::model::{InteractionCallbackData, InteractionResponse};
use bytes::{BufMut, BytesMut};
use futures_util::TryStreamExt;
use hyper::{
    body::{to_bytes, Bytes},
    client::HttpConnector,
    http::{self, uri::InvalidUri},
    Body, Client, Request, Uri,
};
use hyper_tls::HttpsConnector;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub enum FetchError {
    Hyper(hyper::Error),
    Http(hyper::http::Error),
    Json(serde_json::Error),
    Uri(InvalidUri),
}

impl From<http::Error> for FetchError {
    fn from(err: http::Error) -> Self {
        Self::Http(err)
    }
}

impl From<hyper::Error> for FetchError {
    fn from(err: hyper::Error) -> Self {
        Self::Hyper(err)
    }
}

impl From<serde_json::Error> for FetchError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<InvalidUri> for FetchError {
    fn from(err: InvalidUri) -> Self {
        Self::Uri(err)
    }
}

pub struct Fetcher {
    buffer: BytesMut,
    webhook_prefix: Arc<str>,
    application_command_endpoint: Uri,
    client: Client<HttpsConnector<HttpConnector>>,
}

impl Clone for Fetcher {
    fn clone(&self) -> Self {
        Self {
            buffer: BytesMut::new(),
            webhook_prefix: Arc::clone(&self.webhook_prefix),
            application_command_endpoint: self.application_command_endpoint.clone(),
            client: self.client.clone(),
        }
    }
}

impl Fetcher {
    pub fn new(application_id: &str) -> Self {
        let mut https = HttpsConnector::new();
        https.https_only(true);
        let client = Client::builder().build(https);
        let webhook_prefix = format!("https://discord.com/api/webhooks/{}/", application_id).into();
        let application_command_endpoint: Uri =
            format!("https://discord.com/api/applications/{}/commands", application_id)
                .parse()
                .unwrap();
        Self {
            webhook_prefix,
            application_command_endpoint,
            client,
            buffer: Default::default(),
        }
    }

    pub async fn get<'de, T>(&'de mut self, uri: Uri) -> Result<T, FetchError>
    where
        T: Deserialize<'de>,
    {
        let mut body = self.client.get(uri).await?.into_body();

        self.buffer.clear();
        while let Some(bytes) = body.try_next().await? {
            self.buffer.put_slice(&bytes);
        }

        let value = serde_json::from_slice(&self.buffer)?;
        Ok(value)
    }

    pub async fn post<'de, B, R>(&'de mut self, uri: Uri, body: &B) -> Result<R, FetchError>
    where
        B: Serialize,
        R: Deserialize<'de>,
    {
        let body: Body = serde_json::to_vec(body)?.into();
        let req = Request::post(uri).body(body)?;
        let mut res = self.client.request(req).await?.into_body();

        self.buffer.clear();
        while let Some(bytes) = res.try_next().await? {
            self.buffer.put_slice(&bytes);
        }

        let value = serde_json::from_slice(&self.buffer)?;
        Ok(value)
    }

    pub async fn create_followup_message(&mut self, token: &str, content: &str) -> Result<(), FetchError> {
        let uri: Uri = [self.webhook_prefix.as_ref(), token].concat().parse()?;
        let payload = InteractionResponse::ChannelMessageWithSource(InteractionCallbackData { content, flags: 0 });
        self.post(uri, &payload).await
    }

    pub async fn create_application_command(&self) -> Result<Bytes, FetchError> {
        let req = Request::post(self.application_command_endpoint.clone()).body(Body::empty())?;
        todo!()
    }
}
