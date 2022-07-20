// Copyright 2020-2021, The Tremor Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::connectors::impls::gcs::api_client::{
    handle_http_command, FileId, HttpTaskCommand, HttpTaskRequest,
};
use crate::connectors::impls::gcs::chunked_buffer::ChunkedBuffer;
use crate::connectors::prelude::{
    Attempt, ErrorKind, EventSerializer, Result, SinkAddr, SinkContext, SinkManagerBuilder,
    SinkReply, Url,
};
use crate::connectors::sink::{AsyncSinkReply, ContraflowData, Sink};
use crate::connectors::utils::url::HttpsDefaults;
use crate::connectors::{
    Alias, CodecReq, Connector, ConnectorBuilder, ConnectorConfig, ConnectorType, Context,
};
use crate::system::KillSwitch;
use crate::{connectors, QSIZE};
use async_std::channel::{bounded, Receiver, Sender};
#[cfg(not(test))]
use gouth::Token;
use http_client::h1::H1Client;
use http_client::HttpClient;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tremor_common::time::nanotime;
use tremor_pipeline::{ConfigImpl, Event};
use tremor_value::Value;
use value_trait::ValueAccess;

#[derive(Deserialize, Debug, Clone)]
pub struct Config {
    #[serde(default = "default_endpoint")]
    endpoint: Url<HttpsDefaults>,
    // #[cfg_attr(test, allow(unused))]
    #[serde(default = "default_connect_timeout")]
    connect_timeout: u64,
    #[serde(default = "default_buffer_size")]
    buffer_size: usize,
    bucket: Option<String>,
}

#[allow(clippy::unwrap_used)]
fn default_endpoint() -> Url<HttpsDefaults> {
    // ALLOW: this URL is hardcoded, so the only reason for parse failing would be if it was changed
    Url::parse("https://storage.googleapis.com/upload/storage/v1").unwrap()
}

fn default_connect_timeout() -> u64 {
    10_000_000_000
}

fn default_buffer_size() -> usize {
    1024 * 1024 * 8 // 8MB - the recommended minimum
}

impl ConfigImpl for Config {}

#[derive(Debug, Default)]
pub(crate) struct Builder {}

#[async_trait::async_trait]
impl ConnectorBuilder for Builder {
    fn connector_type(&self) -> ConnectorType {
        ConnectorType("gcs_writer".into())
    }

    async fn build_cfg(
        &self,
        _alias: &Alias,
        _config: &ConnectorConfig,
        connector_config: &Value,
        _kill_switch: &KillSwitch,
    ) -> Result<Box<dyn Connector>> {
        let config = Config::new(connector_config)?;

        if config.buffer_size % (256 * 1024) != 0 {
            return Err("Buffer size must be a multiple of 256kiB".into());
        }

        Ok(Box::new(GCSWriterConnector { config }))
    }
}

struct GCSWriterConnector {
    config: Config,
}

#[async_trait::async_trait]
impl Connector for GCSWriterConnector {
    async fn create_sink(
        &mut self,
        sink_context: SinkContext,
        builder: SinkManagerBuilder,
    ) -> Result<Option<SinkAddr>> {
        let default_bucket = self.config.bucket.as_ref().cloned().map(Value::from);

        let reply_tx = builder.reply_tx();

        let sink = GCSWriterSink {
            client_tx: None,
            config: self.config.clone(),
            buffers: ChunkedBuffer::new(self.config.buffer_size),
            current_name: None,
            current_bucket: None,
            default_bucket,
            done_until: Arc::new(AtomicUsize::new(0)),
            reply_tx,
        };

        builder.spawn(sink, sink_context).map(Some)
    }

    fn codec_requirements(&self) -> CodecReq {
        CodecReq::Required
    }
}

fn create_client(connect_timeout: Duration) -> Result<H1Client> {
    let mut client = H1Client::new();
    client.set_config(http_client::Config::new().set_timeout(Some(connect_timeout)))?;

    Ok(client)
}

async fn http_task(
    command_rx: Receiver<HttpTaskRequest>,
    done_until: Arc<AtomicUsize>,
    reply_tx: Sender<AsyncSinkReply>,
    config: Config,
) -> Result<()> {
    let client = create_client(Duration::from_nanos(config.connect_timeout))?;

    #[cfg(not(test))]
    let token = Token::new()?;

    let mut sessions_per_file = HashMap::new();

    while let Ok(request) = command_rx.recv().await {
        let result = handle_http_command(
            done_until.clone(),
            &client,
            &config.endpoint,
            #[cfg(not(test))]
            &token,
            &mut sessions_per_file,
            request.command,
        )
        .await;

        match result {
            Ok(_) => {
                if let Some(contraflow_data) = request.contraflow_data {
                    reply_tx
                        .send(AsyncSinkReply::Ack(
                            contraflow_data,
                            nanotime() - request.start,
                        ))
                        .await?;
                }
            }
            Err(e) => {
                warn!("Failed to handle a message: {:?}", e);
                if let Some(contraflow_data) = request.contraflow_data {
                    reply_tx.send(AsyncSinkReply::Fail(contraflow_data)).await?;
                }
            }
        }
    }

    Ok(())
}

struct GCSWriterSink {
    client_tx: Option<Sender<HttpTaskRequest>>,
    config: Config,
    buffers: ChunkedBuffer,
    current_name: Option<String>,
    current_bucket: Option<String>,
    default_bucket: Option<Value<'static>>,
    done_until: Arc<AtomicUsize>,
    reply_tx: Sender<AsyncSinkReply>,
}

#[async_trait::async_trait]
impl Sink for GCSWriterSink {
    async fn on_event(
        &mut self,
        _input: &str,
        event: Event,
        ctx: &SinkContext,
        serializer: &mut EventSerializer,
        start: u64,
    ) -> Result<SinkReply> {
        self.buffers
            .mark_done_until(self.done_until.load(Ordering::Acquire))?;
        let contraflow_data = ContraflowData::from(&event);

        for (value, meta) in event.value_meta_iter() {
            let meta = ctx.extract_meta(meta);

            let name = meta
                .get("name")
                .ok_or(ErrorKind::GoogleCloudStorageError(
                    "Metadata is missing the file name",
                ))?
                .as_str()
                .ok_or(ErrorKind::GoogleCloudStorageError(
                    "The file name in metadata is not a string",
                ))?;

            self.finish_upload_if_needed(name, Some(contraflow_data.clone()), start)
                .await?;

            self.start_upload_if_needed(meta, name, contraflow_data.clone(), start)
                .await?;

            let serialized_data = serializer.serialize(value, event.ingest_ns)?;
            for item in serialized_data {
                self.buffers.write(&item);
            }

            if let Some(data) = self.buffers.read_current_block() {
                let client_tx = self
                    .client_tx
                    .as_mut()
                    .ok_or(ErrorKind::ClientNotAvailable(
                        "Google Cloud Storage",
                        "not connected",
                    ))?;

                let bucket = get_bucket_name(self.default_bucket.as_ref(), meta)?.to_string();
                self.current_bucket = Some(bucket.clone());

                let command = HttpTaskCommand::UploadData {
                    file: FileId::new(bucket, name),
                    data,
                };
                client_tx
                    .send(HttpTaskRequest {
                        command,
                        start,
                        contraflow_data: Some(contraflow_data.clone()),
                    })
                    .await?;
            }
        }

        Ok(SinkReply::NONE)
    }

    async fn on_stop(&mut self, _ctx: &SinkContext) -> Result<()> {
        self.finish_upload(None, nanotime()).await?;

        Ok(())
    }

    async fn connect(&mut self, ctx: &SinkContext, _attempt: &Attempt) -> Result<bool> {
        let (tx, rx) = bounded(QSIZE.load(Ordering::Relaxed));
        connectors::spawn_task(
            ctx.clone(),
            http_task(
                rx,
                self.done_until.clone(),
                self.reply_tx.clone(),
                self.config.clone(),
            ),
        );

        self.client_tx = Some(tx);

        self.current_name = None;
        self.buffers = ChunkedBuffer::new(self.config.buffer_size);

        Ok(true)
    }

    fn auto_ack(&self) -> bool {
        false
    }
}

impl GCSWriterSink {
    async fn finish_upload_if_needed(
        &mut self,
        name: &str,
        contraflow_data: Option<ContraflowData>,
        start: u64,
    ) -> Result<()> {
        if self.current_name.as_deref() != Some(name) && self.current_name.is_some() {
            return self.finish_upload(contraflow_data, start).await;
        }

        Ok(())
    }

    async fn finish_upload(
        &mut self,
        contraflow_data: Option<ContraflowData>,
        start: u64,
    ) -> Result<()> {
        if let Some(current_name) = self.current_name.as_ref() {
            let client_tx = self
                .client_tx
                .as_mut()
                .ok_or(ErrorKind::ClientNotAvailable(
                    "Google Cloud Storage",
                    "not connected",
                ))?;

            let final_data = self.buffers.final_block();

            let bucket = self
                .current_bucket
                .as_ref()
                .ok_or(ErrorKind::GoogleCloudStorageError(
                    "Current bucket not known",
                ))?;
            let command = HttpTaskCommand::FinishUpload {
                file: FileId::new(bucket, current_name),
                data: final_data,
            };

            client_tx
                .send(HttpTaskRequest {
                    command,
                    contraflow_data,
                    start,
                })
                .await?;

            self.buffers = ChunkedBuffer::new(self.config.buffer_size);
            self.current_name = None;
        }

        Ok(())
    }

    async fn start_upload_if_needed(
        &mut self,
        meta: Option<&Value<'_>>,
        name: &str,
        contraflow_data: ContraflowData,
        start: u64,
    ) -> Result<()> {
        let client_tx = self
            .client_tx
            .as_mut()
            .ok_or(ErrorKind::ClientNotAvailable(
                "Google Cloud Storage",
                "not connected",
            ))?;

        if self.current_name.is_none() {
            let bucket = get_bucket_name(self.default_bucket.as_ref(), meta)?;
            self.current_bucket = Some(bucket.clone());

            let command = HttpTaskCommand::StartUpload {
                file: FileId::new(bucket, name),
            };
            client_tx
                .send(HttpTaskRequest {
                    command,
                    start,
                    contraflow_data: Some(contraflow_data),
                })
                .await?;

            self.current_name = Some(name.to_string());
        }

        Ok(())
    }
}

fn get_bucket_name(
    default_bucket: Option<&Value<'static>>,
    meta: Option<&Value>,
) -> Result<String> {
    let bucket = meta
        .get("bucket")
        .or(default_bucket)
        .ok_or(ErrorKind::GoogleCloudStorageError(
            "No bucket name in the metadata",
        ))
        .as_str()
        .ok_or(ErrorKind::GoogleCloudStorageError(
            "Bucket name is not a string",
        ))?
        .to_string();

    Ok(bucket)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Codec;
    use crate::connectors::impls::gcs::chunked_buffer::BufferPart;
    use crate::connectors::reconnect::ConnectionLostNotifier;
    use beef::Cow;
    use tremor_script::{EventPayload, ValueAndMeta};
    use tremor_value::literal;

    #[async_std::test]
    pub async fn fails_when_buffer_size_is_not_divisible_by_256ki() {
        let raw_config = literal!({
            "buffer_size": 256 * 1000
        });

        let builder = Builder {};
        let result = builder
            .build_cfg(
                &Alias::new("", ""),
                &ConnectorConfig {
                    connector_type: Default::default(),
                    codec: None,
                    config: None,
                    preprocessors: None,
                    postprocessors: None,
                    reconnect: Default::default(),
                    metrics_interval_s: None,
                },
                &raw_config,
                &KillSwitch::dummy(),
            )
            .await;

        assert!(result.is_err());
    }

    #[async_std::test]
    pub async fn starts_upload_on_first_event() {
        let (client_tx, client_rx) = bounded(10);
        let (reply_tx, _) = bounded(10);

        let mut sink = GCSWriterSink {
            client_tx: Some(client_tx),
            config: Config {
                endpoint: Default::default(),
                connect_timeout: 1000000000,
                buffer_size: 10,
                bucket: None,
            },
            buffers: ChunkedBuffer::new(10),
            current_name: None,
            current_bucket: None,
            default_bucket: None,
            done_until: Arc::new(Default::default()),
            reply_tx,
        };

        let (connection_lost_tx, _) = bounded(10);

        let alias = Alias::new("a", "b");
        let context = SinkContext {
            uid: Default::default(),
            alias: alias.clone(),
            connector_type: "gcs_writer".into(),
            quiescence_beacon: Default::default(),
            notifier: ConnectionLostNotifier::new(connection_lost_tx),
        };
        let mut serializer = EventSerializer::new(
            Some(Codec::from("json")),
            CodecReq::Required,
            vec![],
            &"gcs_writer".into(),
            &alias,
        )
        .unwrap();

        let value = literal!({});
        let meta = literal!({
            "gcs_writer": {
                "name": "test.txt",
                "bucket": "woah"
            }
        });

        let event_payload = EventPayload::from(ValueAndMeta::from_parts(value, meta));

        let event = Event {
            id: Default::default(),
            data: event_payload,
            ingest_ns: 0,
            origin_uri: None,
            kind: None,
            is_batch: false,
            cb: Default::default(),
            op_meta: Default::default(),
            transactional: false,
        };
        sink.on_event("", event.clone(), &context, &mut serializer, 1234)
            .await
            .unwrap();

        let response = client_rx.try_recv().unwrap();

        assert_eq!(
            response.command,
            HttpTaskCommand::StartUpload {
                file: FileId::new("woah", "test.txt")
            }
        );
        assert_eq!(response.start, 1234);
        assert!(response.contraflow_data.is_some());
    }

    #[async_std::test]
    pub async fn uploads_data_when_the_buffer_gets_big_enough() {
        let (client_tx, client_rx) = bounded(10);
        let (reply_tx, _) = bounded(10);

        let mut sink = GCSWriterSink {
            client_tx: Some(client_tx),
            config: Config {
                endpoint: Default::default(),
                connect_timeout: 1000000000,
                buffer_size: 10,
                bucket: None,
            },
            buffers: ChunkedBuffer::new(10),
            current_name: None,
            current_bucket: None,
            default_bucket: None,
            done_until: Arc::new(Default::default()),
            reply_tx,
        };

        let (connection_lost_tx, _) = bounded(10);

        let alias = Alias::new("a", "b");
        let context = SinkContext {
            uid: Default::default(),
            alias: alias.clone(),
            connector_type: "gcs_writer".into(),
            quiescence_beacon: Default::default(),
            notifier: ConnectionLostNotifier::new(connection_lost_tx),
        };
        let mut serializer = EventSerializer::new(
            Some(Codec::from("binary")),
            CodecReq::Required,
            vec![],
            &"gcs_writer".into(),
            &alias,
        )
        .unwrap();

        let value = Value::Bytes(Cow::from(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
        let meta = literal!({
            "gcs_writer": {
                "name": "test.txt",
                "bucket": "woah"
            }
        });

        let event_payload = EventPayload::from(ValueAndMeta::from_parts(value, meta));

        let event = Event {
            id: Default::default(),
            data: event_payload,
            ingest_ns: 0,
            origin_uri: None,
            kind: None,
            is_batch: false,
            cb: Default::default(),
            op_meta: Default::default(),
            transactional: false,
        };
        sink.on_event("", event.clone(), &context, &mut serializer, 1234)
            .await
            .unwrap();

        // ignore the upload start
        let _ = client_rx.try_recv().unwrap();

        let response = client_rx.try_recv().unwrap();

        assert_eq!(
            response.command,
            HttpTaskCommand::UploadData {
                file: FileId::new("woah", "test.txt"),
                data: BufferPart {
                    data: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
                    start: 0
                }
            }
        );
        assert_eq!(response.start, 1234);
        assert!(response.contraflow_data.is_some());
    }

    #[async_std::test]
    pub async fn finishes_upload_on_filename_change() {
        let (client_tx, client_rx) = bounded(10);
        let (reply_tx, _) = bounded(10);

        let mut sink = GCSWriterSink {
            client_tx: Some(client_tx),
            config: Config {
                endpoint: Default::default(),
                connect_timeout: 1000000000,
                buffer_size: 10,
                bucket: None,
            },
            buffers: ChunkedBuffer::new(10),
            current_name: None,
            current_bucket: None,
            default_bucket: None,
            done_until: Arc::new(Default::default()),
            reply_tx,
        };

        let (connection_lost_tx, _) = bounded(10);

        let alias = Alias::new("a", "b");
        let context = SinkContext {
            uid: Default::default(),
            alias: alias.clone(),
            connector_type: "gcs_writer".into(),
            quiescence_beacon: Default::default(),
            notifier: ConnectionLostNotifier::new(connection_lost_tx),
        };
        let mut serializer = EventSerializer::new(
            Some(Codec::from("json")),
            CodecReq::Required,
            vec![],
            &"gcs_writer".into(),
            &alias,
        )
        .unwrap();

        let value = literal!({});
        let meta = literal!({
            "gcs_writer": {
                "name": "test.txt",
                "bucket": "woah"
            }
        });

        let event_payload = EventPayload::from(ValueAndMeta::from_parts(value, meta));

        let event = Event {
            id: Default::default(),
            data: event_payload,
            ingest_ns: 0,
            origin_uri: None,
            kind: None,
            is_batch: false,
            cb: Default::default(),
            op_meta: Default::default(),
            transactional: false,
        };
        sink.on_event("", event.clone(), &context, &mut serializer, 1234)
            .await
            .unwrap();
        let value = literal!({});
        let meta = literal!({
            "gcs_writer": {
                "name": "test_other.txt",
                "bucket": "woah"
            }
        });

        let event_payload = EventPayload::from(ValueAndMeta::from_parts(value, meta));

        let event = Event {
            id: Default::default(),
            data: event_payload,
            ingest_ns: 0,
            origin_uri: None,
            kind: None,
            is_batch: false,
            cb: Default::default(),
            op_meta: Default::default(),
            transactional: false,
        };
        sink.on_event("", event.clone(), &context, &mut serializer, 1234)
            .await
            .unwrap();

        // ignore the first event - upload start
        let _ = client_rx.try_recv().unwrap();

        let response = client_rx.try_recv().unwrap();

        assert_eq!(
            response.command,
            HttpTaskCommand::FinishUpload {
                file: FileId::new("woah", "test.txt"),
                data: BufferPart {
                    data: b"{}".to_vec(),
                    start: 0,
                }
            }
        );
        assert_eq!(response.start, 1234);
        assert!(response.contraflow_data.is_some());

        let response = client_rx.try_recv().unwrap();

        assert_eq!(
            response.command,
            HttpTaskCommand::StartUpload {
                file: FileId::new("woah", "test_other.txt")
            }
        );
        assert_eq!(response.start, 1234);
        assert!(response.contraflow_data.is_some());
    }
}