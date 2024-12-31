use crate::dispatcher::Message;
use crate::types::{BrokerSocketOutgoingReceiver, DispatcherSender};
use anyhow::{Context as _, Error, Result};
use maelstrom_base::proto;
use maelstrom_base::proto::Hello;
use maelstrom_github::{GitHubQueue, GitHubReadQueue, GitHubWriteQueue};
use maelstrom_util::{
    config::common::{BrokerAddr, Slots},
    net::{self, AsRawFdExt as _},
};
use slog::{debug, error, Logger};
use std::future::Future;
use tokio::{io::BufReader, net::TcpStream};

pub trait BrokerConnection: Sized {
    type Read: BrokerReadConnection;
    type Write: BrokerWriteConnection;

    async fn connect(
        addr: &BrokerAddr,
        slots: Slots,
        log: &Logger,
    ) -> Result<(Self::Read, Self::Write)>;
}

impl BrokerConnection for TcpStream {
    type Read = BufReader<tokio::net::tcp::OwnedReadHalf>;
    type Write = tokio::net::tcp::OwnedWriteHalf;

    async fn connect(
        addr: &BrokerAddr,
        slots: Slots,
        log: &Logger,
    ) -> Result<(Self::Read, Self::Write)> {
        let (read, mut write) = TcpStream::connect(addr.inner())
            .await
            .map_err(|err| {
                error!(log, "error connecting to broker"; "error" => %err);
                err
            })?
            .set_socket_options()?
            .into_split();

        net::write_message_to_async_socket(
            &mut write,
            Hello::Worker {
                slots: slots.into_inner().into(),
            },
            log,
        )
        .await?;

        Ok((BufReader::new(read), write))
    }
}

pub trait BrokerReadConnection: Send + Sync + 'static {
    fn read_messages(
        self,
        dispatcher_sender: DispatcherSender,
        log: Logger,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl BrokerReadConnection for BufReader<tokio::net::tcp::OwnedReadHalf> {
    async fn read_messages(self, dispatcher_sender: DispatcherSender, log: Logger) -> Result<()> {
        net::async_socket_reader(self, dispatcher_sender, Message::Broker, &log)
            .await
            .context("error communicating with broker")
    }
}

pub trait BrokerWriteConnection: Send + Sync + 'static {
    fn write_messages(
        self,
        broker_socket_outgoing_receiver: BrokerSocketOutgoingReceiver,
        log: Logger,
    ) -> impl Future<Output = Result<()>> + Send;
}

impl BrokerWriteConnection for tokio::net::tcp::OwnedWriteHalf {
    async fn write_messages(
        self,
        broker_socket_outgoing_receiver: BrokerSocketOutgoingReceiver,
        log: Logger,
    ) -> Result<()> {
        net::async_socket_writer(broker_socket_outgoing_receiver, self, &log)
            .await
            .context("error communicating with broker")
    }
}

impl BrokerConnection for GitHubQueue {
    type Read = GitHubReadQueue;
    type Write = GitHubWriteQueue;

    async fn connect(
        _addr: &BrokerAddr,
        slots: Slots,
        log: &Logger,
    ) -> Result<(Self::Read, Self::Write)> {
        let client = crate::github_client_factory()?;
        let (read, mut write) = GitHubQueue::connect(&*client, "maelstrom-broker")
            .await
            .map_err(|err| {
                error!(log, "error connecting to broker"; "error" => %err);
                err
            })?
            .into_split();

        write
            .write_msg(
                &proto::serialize(&Hello::Worker {
                    slots: slots.into_inner().into(),
                })
                .unwrap(),
            )
            .await?;

        Ok((read, write))
    }
}

impl BrokerReadConnection for GitHubReadQueue {
    async fn read_messages(
        mut self,
        dispatcher_sender: DispatcherSender,
        log: Logger,
    ) -> Result<()> {
        loop {
            let msg = async {
                self.read_msg()
                    .await?
                    .as_ref()
                    .map(|m| proto::deserialize(m))
                    .transpose()
                    .map_err(Error::from)
            }
            .await
            .inspect_err(|err| debug!(log, "error receiving message"; "error" => %err))
            .context("error communicating with broker")?;
            if let Some(msg) = msg {
                debug!(log, "received message"; "message" => #?msg);
                if dispatcher_sender.send(Message::Broker(msg)).is_err() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(())
    }
}

impl BrokerWriteConnection for GitHubWriteQueue {
    async fn write_messages(
        mut self,
        mut broker_socket_outgoing_receiver: BrokerSocketOutgoingReceiver,
        log: Logger,
    ) -> Result<()> {
        while let Some(msg) = broker_socket_outgoing_receiver.recv().await {
            debug!(log, "sending message"; "message" => #?msg);
            self.write_msg(&proto::serialize(&msg).unwrap())
                .await
                .inspect_err(|err| debug!(log, "error sending message"; "error" => %err))?;
        }
        Ok(())
    }
}
