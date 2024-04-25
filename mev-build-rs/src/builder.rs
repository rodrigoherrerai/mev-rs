use crate::{
    auction_schedule::{Proposals, Proposer},
    auctioneer::Message as AuctioneerMessage,
    bidder::AuctionContext,
    payload::builder_attributes::{BuilderPayloadBuilderAttributes, ProposalAttributes},
    Error,
};
use alloy_signer_wallet::{coins_bip39::English, LocalWallet, MnemonicBuilder};
use ethereum_consensus::{
    clock::convert_timestamp_to_slot, primitives::Slot, state_transition::Context,
};
use reth::{
    api::{EngineTypes, PayloadBuilderAttributes},
    payload::{EthBuiltPayload, Events, PayloadBuilderHandle, PayloadId, PayloadStore},
    primitives::{Address, Bytes},
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};
use tokio_stream::StreamExt;
use tracing::{error, warn};

fn make_attributes_for_proposer(
    attributes: &BuilderPayloadBuilderAttributes,
    builder_fee_recipient: Address,
    builder_signer: Arc<LocalWallet>,
    proposer: &Proposer,
) -> BuilderPayloadBuilderAttributes {
    let proposal = ProposalAttributes {
        builder_fee_recipient,
        builder_signer,
        suggested_gas_limit: proposer.gas_limit,
        proposer_fee_recipient: proposer.fee_recipient,
    };
    let mut attributes = attributes.clone();
    attributes.attach_proposal(proposal);
    attributes
}

pub enum KeepAlive {
    No,
}

pub enum Message {
    FetchPayload(PayloadId, KeepAlive),
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct Config {
    pub fee_recipient: Address,
    pub genesis_time: Option<u64>,
    pub extra_data: Option<Bytes>,
    pub execution_mnemonic: String,
}

pub struct Builder<
    Engine: EngineTypes<
        PayloadBuilderAttributes = BuilderPayloadBuilderAttributes,
        BuiltPayload = EthBuiltPayload,
    >,
> {
    msgs: Receiver<Message>,
    auctioneer: Sender<AuctioneerMessage>,
    payload_builder: PayloadBuilderHandle<Engine>,
    payload_store: PayloadStore<Engine>,
    config: Config,
    context: Arc<Context>,
    genesis_time: u64,
    signer: Arc<LocalWallet>,
}

impl<
        Engine: EngineTypes<
                PayloadBuilderAttributes = BuilderPayloadBuilderAttributes,
                BuiltPayload = EthBuiltPayload,
            > + 'static,
    > Builder<Engine>
{
    pub fn new(
        msgs: Receiver<Message>,
        auctioneer: Sender<AuctioneerMessage>,
        payload_builder: PayloadBuilderHandle<Engine>,
        config: Config,
        context: Arc<Context>,
        genesis_time: u64,
    ) -> Self {
        let payload_store = payload_builder.clone().into();
        let signer = MnemonicBuilder::<English>::default()
            .phrase(&config.execution_mnemonic)
            .build()
            .expect("is valid");
        Self {
            msgs,
            auctioneer,
            payload_builder,
            payload_store,
            config,
            context,
            genesis_time,
            signer: Arc::new(signer),
        }
    }

    pub async fn process_proposals(
        &self,
        slot: Slot,
        attributes: BuilderPayloadBuilderAttributes,
        proposals: Option<Proposals>,
    ) -> Result<Vec<AuctionContext>, Error> {
        let mut new_auctions = vec![];

        if let Some(proposals) = proposals {
            for (proposer, relays) in proposals {
                let attributes = make_attributes_for_proposer(
                    &attributes,
                    self.config.fee_recipient,
                    self.signer.clone(),
                    &proposer,
                );

                if self.start_build(&attributes).await.is_some() {
                    // TODO: can likely skip full attributes in `AuctionContext`
                    let auction = AuctionContext { slot, attributes, proposer, relays };
                    new_auctions.push(auction);
                }
            }
        }
        Ok(new_auctions)
    }

    // TODO: can likely skip returning attributes here...
    async fn start_build(&self, attributes: &BuilderPayloadBuilderAttributes) -> Option<PayloadId> {
        match self.payload_builder.new_payload(attributes.clone()).await {
            Ok(payload_id) => {
                let attributes_payload_id = attributes.payload_id();
                if payload_id != attributes_payload_id {
                    error!(%payload_id, %attributes_payload_id, "mismatch between computed payload id and the one returned by the payload builder");
                }
                Some(payload_id)
            }
            Err(err) => {
                warn!(%err, "builder could not start build with payload builder");
                None
            }
        }
    }

    async fn on_payload_attributes(&self, attributes: BuilderPayloadBuilderAttributes) {
        // TODO: ignore already processed attributes

        // TODO: move slot calc to auctioneer?
        let slot = convert_timestamp_to_slot(
            attributes.timestamp(),
            self.genesis_time,
            self.context.seconds_per_slot,
        )
        .expect("is past genesis");
        let (tx, rx) = oneshot::channel();
        self.auctioneer.send(AuctioneerMessage::ProposalQuery(slot, tx)).await.expect("can send");
        let proposals = rx.await.expect("can recv");
        let auctions = self.process_proposals(slot, attributes, proposals).await;
        match auctions {
            Ok(auctions) => {
                self.auctioneer
                    .send(AuctioneerMessage::NewAuctions(auctions))
                    .await
                    .expect("can send");
            }
            Err(err) => {
                warn!(%err, "could not send new auctions to auctioneer");
            }
        }
    }

    async fn send_payload_to_auctioneer(&self, payload_id: PayloadId, _keep_alive: KeepAlive) {
        // TODO: put into separate task?
        // TODO: signal to payload job `_keep_alive` status
        let maybe_payload = self.payload_store.resolve(payload_id).await;
        if let Some(payload) = maybe_payload {
            match payload {
                // TODO: auctioneer can just listen for payload events instead
                Ok(payload) => self
                    .auctioneer
                    .send(AuctioneerMessage::BuiltPayload(payload))
                    .await
                    .expect("can send"),
                Err(err) => {
                    warn!(%err, %payload_id, "error resolving payload")
                }
            }
        } else {
            warn!(%payload_id, "could not resolve payload")
        }
    }

    async fn dispatch(&self, message: Message) {
        match message {
            Message::FetchPayload(payload_id, keep_alive) => {
                self.send_payload_to_auctioneer(payload_id, keep_alive).await;
            }
        }
    }

    pub async fn spawn(mut self) {
        let mut payload_events =
            self.payload_builder.subscribe().await.expect("can subscribe to events").into_stream();
        loop {
            tokio::select! {
                Some(message) = self.msgs.recv() => self.dispatch(message).await,
                Some(Ok(Events::Attributes(attributes))) = payload_events.next() => self.on_payload_attributes(attributes).await,
            }
        }
    }
}
