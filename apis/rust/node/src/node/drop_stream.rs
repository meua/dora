use std::time::Duration;

use crate::daemon_connection::DaemonChannel;
use dora_core::{
    config::NodeId,
    daemon_messages::{
        self, DaemonCommunication, DaemonReply, DaemonRequest, DataflowId, DropToken, NodeDropEvent,
    },
};
use eyre::{eyre, Context};
use flume::RecvTimeoutError;

pub struct DropStream {
    receiver: flume::Receiver<DropToken>,
    _thread_handle: DropStreamThreadHandle,
}

impl DropStream {
    #[tracing::instrument(level = "trace")]
    pub(crate) fn init(
        dataflow_id: DataflowId,
        node_id: &NodeId,
        daemon_communication: &DaemonCommunication,
    ) -> eyre::Result<Self> {
        let channel = match daemon_communication {
            DaemonCommunication::Shmem {
                daemon_drop_region_id,
                ..
            } => {
                unsafe { DaemonChannel::new_shmem(daemon_drop_region_id) }.wrap_err_with(|| {
                    format!("failed to create shmem drop stream for node `{node_id}`")
                })?
            }
            DaemonCommunication::Tcp { socket_addr } => DaemonChannel::new_tcp(*socket_addr)
                .wrap_err_with(|| format!("failed to connect drop stream for node `{node_id}`"))?,
        };

        Self::init_on_channel(dataflow_id, node_id, channel)
    }

    pub fn init_on_channel(
        dataflow_id: DataflowId,
        node_id: &NodeId,
        mut channel: DaemonChannel,
    ) -> eyre::Result<Self> {
        channel.register(dataflow_id, node_id.clone())?;

        let reply = channel
            .request(&DaemonRequest::SubscribeDrop)
            .map_err(|e| eyre!(e))
            .wrap_err("failed to create subscription with dora-daemon")?;

        match reply {
            daemon_messages::DaemonReply::Result(Ok(())) => {}
            daemon_messages::DaemonReply::Result(Err(err)) => {
                eyre::bail!("drop subscribe failed: {err}")
            }
            other => eyre::bail!("unexpected drop subscribe reply: {other:?}"),
        }

        let (tx, rx) = flume::bounded(0);
        let node_id_cloned = node_id.clone();

        let handle = std::thread::spawn(|| drop_stream_loop(node_id_cloned, tx, channel));

        Ok(Self {
            receiver: rx,
            _thread_handle: DropStreamThreadHandle::new(node_id.clone(), handle),
        })
    }
}

impl std::ops::Deref for DropStream {
    type Target = flume::Receiver<DropToken>;

    fn deref(&self) -> &Self::Target {
        &self.receiver
    }
}

#[tracing::instrument(skip(tx, channel))]
fn drop_stream_loop(node_id: NodeId, tx: flume::Sender<DropToken>, mut channel: DaemonChannel) {
    'outer: loop {
        let daemon_request = DaemonRequest::NextFinishedDropTokens;
        let events = match channel.request(&daemon_request) {
            Ok(DaemonReply::NextDropEvents(events)) if events.is_empty() => {
                tracing::trace!("drop stream closed for node `{node_id}`");
                break;
            }
            Ok(DaemonReply::NextDropEvents(events)) => events,
            Ok(other) => {
                let err = eyre!("unexpected drop reply: {other:?}");
                tracing::warn!("{err:?}");
                continue;
            }
            Err(err) => {
                let err = eyre!(err).wrap_err("failed to receive incoming drop event");
                tracing::warn!("{err:?}");
                continue;
            }
        };
        for event in events {
            match event {
                NodeDropEvent::OutputDropped { drop_token } => {
                    if tx.send(drop_token).is_err() {
                        tracing::warn!(
                            "drop channel was closed already, could not forward \
                            drop token`{drop_token:?}`"
                        );
                        break 'outer;
                    }
                }
            }
        }
    }
}

struct DropStreamThreadHandle {
    node_id: NodeId,
    handle: flume::Receiver<std::thread::Result<()>>,
}

impl DropStreamThreadHandle {
    fn new(node_id: NodeId, join_handle: std::thread::JoinHandle<()>) -> Self {
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(join_handle.join());
        });
        Self {
            node_id,
            handle: rx,
        }
    }
}

impl Drop for DropStreamThreadHandle {
    #[tracing::instrument(skip(self), fields(node_id = %self.node_id))]
    fn drop(&mut self) {
        if self.handle.is_empty() {
            tracing::trace!("waiting for drop stream thread");
        }
        match self.handle.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => {
                tracing::trace!("drop stream thread done");
            }
            Ok(Err(_)) => {
                tracing::error!("drop stream thread panicked");
            }
            Err(RecvTimeoutError::Timeout) => {
                tracing::warn!("timeout while waiting for drop stream thread");
            }
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!("drop stream thread result channel closed unexpectedly");
            }
        }
    }
}
