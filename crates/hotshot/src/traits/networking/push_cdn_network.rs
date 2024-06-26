use super::NetworkError;
#[cfg(feature = "hotshot-testing")]
use async_compatibility_layer::art::async_spawn;
use async_compatibility_layer::channel::UnboundedSendError;
use async_trait::async_trait;
use bincode::config::Options;
use cdn_broker::reexports::connection::protocols::Tcp;
use cdn_broker::reexports::def::RunDef;
use cdn_broker::reexports::discovery::{Embedded, Redis};
#[cfg(feature = "hotshot-testing")]
use cdn_broker::{Broker, Config, ConfigBuilder as BrokerConfigBuilder};
pub use cdn_client::reexports::crypto::signature::KeyPair;
use cdn_client::{
    reexports::{
        connection::protocols::Quic,
        crypto::signature::{Serializable, SignatureScheme},
        message::{Broadcast, Direct, Message as PushCdnMessage, Topic},
    },
    Client, ConfigBuilder as ClientConfigBuilder,
};
#[cfg(feature = "hotshot-testing")]
use cdn_marshal::{ConfigBuilder as MarshalConfigBuilder, Marshal};
use hotshot_types::traits::network::AsyncGenerator;
#[cfg(feature = "hotshot-testing")]
use hotshot_types::traits::network::{NetworkReliability, TestableNetworkingImplementation};
use hotshot_types::{
    boxed_sync,
    constants::{Version01, VERSION_0_1},
    data::ViewNumber,
    message::Message,
    traits::{
        network::{ConnectedNetwork, ConsensusIntentEvent, PushCdnNetworkError},
        node_implementation::NodeType,
        signature_key::SignatureKey,
    },
    utils::bincode_opts,
    BoxSyncFuture,
};
#[cfg(feature = "hotshot-testing")]
use rand::rngs::StdRng;
#[cfg(feature = "hotshot-testing")]
use rand::{RngCore, SeedableRng};
use std::collections::BTreeSet;
use std::marker::PhantomData;
#[cfg(feature = "hotshot-testing")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{path::Path, sync::Arc, time::Duration};
use tracing::{error, warn};
use versioned_binary_serialization::{
    version::{StaticVersionType, Version},
    BinarySerializer, Serializer,
};

/// A wrapped `SignatureKey`. We need to implement the Push CDN's `SignatureScheme`
/// trait in order to sign and verify messages to/from the CDN.
#[derive(Clone, Eq, PartialEq)]
pub struct WrappedSignatureKey<T: SignatureKey + 'static>(pub T);
impl<T: SignatureKey> SignatureScheme for WrappedSignatureKey<T> {
    type PrivateKey = T::PrivateKey;
    type PublicKey = Self;

    /// Sign a message of arbitrary data and return the serialized signature
    fn sign(private_key: &Self::PrivateKey, message: &[u8]) -> anyhow::Result<Vec<u8>> {
        let signature = T::sign(private_key, message)?;
        // TODO: replace with rigorously defined serialization scheme...
        // why did we not make `PureAssembledSignatureType` be `CanonicalSerialize + CanonicalDeserialize`?
        Ok(bincode_opts().serialize(&signature)?)
    }

    /// Verify a message of arbitrary data and return the result
    fn verify(public_key: &Self::PublicKey, message: &[u8], signature: &[u8]) -> bool {
        // TODO: replace with rigorously defined signing scheme
        let signature: T::PureAssembledSignatureType = match bincode_opts().deserialize(signature) {
            Ok(key) => key,
            Err(_) => return false,
        };

        public_key.0.validate(&signature, message)
    }
}

/// We need to implement the `Serializable` so the Push CDN can serialize the signatures
/// and public keys and send them over the wire.
impl<T: SignatureKey> Serializable for WrappedSignatureKey<T> {
    fn serialize(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self.0.to_bytes())
    }

    fn deserialize(serialized: &[u8]) -> anyhow::Result<Self> {
        Ok(WrappedSignatureKey(T::from_bytes(serialized)?))
    }
}

/// The testing run definition for the Push CDN.
/// Uses the real protocols, but with an embedded discovery client.
pub struct TestingDef<TYPES: NodeType> {
    /// Phantom data to hold the type
    pd: PhantomData<TYPES>,
}

impl<TYPES: NodeType> RunDef for TestingDef<TYPES> {
    type BrokerScheme = WrappedSignatureKey<TYPES::SignatureKey>;
    type BrokerProtocol = Tcp;

    type UserScheme = WrappedSignatureKey<TYPES::SignatureKey>;
    type UserProtocol = Quic;

    type DiscoveryClientType = Embedded;
}

/// The production run definition for the Push CDN.
/// Uses the real protocols and a Redis discovery client.
pub struct ProductionDef<TYPES: NodeType> {
    /// Phantom data to hold the type
    pd: PhantomData<TYPES>,
}

impl<TYPES: NodeType> RunDef for ProductionDef<TYPES> {
    type BrokerScheme = WrappedSignatureKey<TYPES::SignatureKey>;
    type BrokerProtocol = Tcp;

    type UserScheme = WrappedSignatureKey<TYPES::SignatureKey>;
    type UserProtocol = Quic;

    type DiscoveryClientType = Redis;
}

/// A communication channel to the Push CDN, which is a collection of brokers and a marshal
/// that helps organize them all.
#[derive(Clone)]
/// Is generic over both the type of key and the network protocol.
pub struct PushCdnNetwork<TYPES: NodeType> {
    /// The underlying client
    client: Client<WrappedSignatureKey<TYPES::SignatureKey>, Quic>,
    /// Whether or not the underlying network is supposed to be paused
    #[cfg(feature = "hotshot-testing")]
    is_paused: Arc<AtomicBool>,
}

impl<TYPES: NodeType> PushCdnNetwork<TYPES> {
    /// Create a new `PushCdnNetwork` (really a client) from a marshal endpoint, a list of initial
    /// topics we are interested in, and our wrapped keypair that we use to authenticate with the
    /// marshal.
    ///
    /// # Errors
    /// If we fail the initial connection
    pub async fn new(
        marshal_endpoint: String,
        topics: Vec<String>,
        keypair: KeyPair<WrappedSignatureKey<TYPES::SignatureKey>>,
    ) -> anyhow::Result<Self> {
        // Transform topics to our internal representation
        let mut computed_topics: Vec<Topic> = Vec::new();
        for topic in topics {
            computed_topics.push(topic.try_into()?);
        }

        // Build config
        let config = ClientConfigBuilder::default()
            .endpoint(marshal_endpoint)
            .subscribed_topics(computed_topics)
            .keypair(keypair)
            .build()?;

        // Create the client, performing the initial connection
        let client = Client::new(config).await?;

        Ok(Self {
            client,
            // Start unpaused
            #[cfg(feature = "hotshot-testing")]
            is_paused: Arc::from(AtomicBool::new(false)),
        })
    }

    /// Broadcast a message to members of the particular topic. Does not retry.
    ///
    /// # Errors
    /// - If we fail to serialize the message
    /// - If we fail to send the broadcast message.
    async fn broadcast_message<Ver: StaticVersionType>(
        &self,
        message: Message<TYPES>,
        topic: Topic,
        _: Ver,
    ) -> Result<(), NetworkError> {
        // If we're paused, don't send the message
        #[cfg(feature = "hotshot-testing")]
        if self.is_paused.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Bincode the message
        let serialized_message = match Serializer::<Ver>::serialize(&message) {
            Ok(serialized) => serialized,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                return Err(NetworkError::FailedToSerialize { source: e });
            }
        };

        // Send the message
        // TODO: check if we need to print this error
        if self
            .client
            .send_broadcast_message(vec![topic], serialized_message)
            .await
            .is_err()
        {
            return Err(NetworkError::CouldNotDeliver);
        };

        Ok(())
    }
}

#[cfg(feature = "hotshot-testing")]
impl<TYPES: NodeType> TestableNetworkingImplementation<TYPES> for PushCdnNetwork<TYPES> {
    /// Generate n Push CDN clients, a marshal, and two brokers (that run locally).
    /// Uses a `SQLite` database instead of Redis.
    fn generator(
        _expected_node_count: usize,
        _num_bootstrap: usize,
        _network_id: usize,
        da_committee_size: usize,
        _is_da: bool,
        _reliability_config: Option<Box<dyn NetworkReliability>>,
        _secondary_network_delay: Duration,
    ) -> AsyncGenerator<(Arc<Self>, Arc<Self>)> {
        // The configuration we are using for testing is 2 brokers & 1 marshal

        // A keypair shared between brokers
        let (broker_public_key, broker_private_key) =
            TYPES::SignatureKey::generated_from_seed_indexed([0u8; 32], 1337);

        // Get the OS temporary directory
        let temp_dir = std::env::temp_dir();

        // Create an SQLite file inside of the temporary directory
        let discovery_endpoint = temp_dir
            .join(Path::new(&format!(
                "test-{}.sqlite",
                StdRng::from_entropy().next_u64()
            )))
            .to_string_lossy()
            .into_owned();

        // 2 brokers
        for _ in 0..2 {
            // Get the ports to bind to
            let private_port = portpicker::pick_unused_port().expect("could not find an open port");
            let public_port = portpicker::pick_unused_port().expect("could not find an open port");

            // Extrapolate addresses
            let private_address = format!("127.0.0.1:{private_port}");
            let public_address = format!("127.0.0.1:{public_port}");

            let config: Config<WrappedSignatureKey<TYPES::SignatureKey>> =
                BrokerConfigBuilder::default()
                    .discovery_endpoint(discovery_endpoint.clone())
                    .keypair(KeyPair {
                        public_key: WrappedSignatureKey(broker_public_key.clone()),
                        private_key: broker_private_key.clone(),
                    })
                    .metrics_enabled(false)
                    .private_bind_address(private_address.clone())
                    .public_bind_address(public_address.clone())
                    .private_advertise_address(private_address)
                    .public_advertise_address(public_address)
                    .build()
                    .expect("failed to build broker config");

            // Create and spawn the broker
            async_spawn(async move {
                let broker: Broker<TestingDef<TYPES>> =
                    Broker::new(config).await.expect("broker failed to start");

                // Error if we stopped unexpectedly
                if let Err(err) = broker.start().await {
                    error!("broker stopped: {err}");
                }
            });
        }

        // Get the port to use for the marshal
        let marshal_port = portpicker::pick_unused_port().expect("could not find an open port");

        // Configure the marshal
        let marshal_endpoint = format!("127.0.0.1:{marshal_port}");
        let marshal_config = MarshalConfigBuilder::default()
            .bind_address(marshal_endpoint.clone())
            .metrics_enabled(false)
            .discovery_endpoint(discovery_endpoint)
            .build()
            .expect("failed to build marshal config");

        // Spawn the marshal
        async_spawn(async move {
            let marshal: Marshal<TestingDef<TYPES>> = Marshal::new(marshal_config)
                .await
                .expect("failed to spawn marshal");

            // Error if we stopped unexpectedly
            if let Err(err) = marshal.start().await {
                error!("broker stopped: {err}");
            }
        });

        // This function is called for each client we spawn
        Box::pin({
            move |node_id| {
                // Clone this so we can pin the future
                let marshal_endpoint = marshal_endpoint.clone();

                Box::pin(async move {
                    // Derive our public and priate keys from our index
                    let private_key =
                        TYPES::SignatureKey::generated_from_seed_indexed([0u8; 32], node_id).1;
                    let public_key = TYPES::SignatureKey::from_private(&private_key);

                    // Calculate if we're DA or not
                    let topics = if node_id < da_committee_size as u64 {
                        vec![Topic::DA, Topic::Global]
                    } else {
                        vec![Topic::Global]
                    };

                    // Configure our client
                    let client_config = ClientConfigBuilder::default()
                        .keypair(KeyPair {
                            public_key: WrappedSignatureKey(public_key),
                            private_key,
                        })
                        .subscribed_topics(topics)
                        .endpoint(marshal_endpoint)
                        .build()
                        .expect("failed to build client config");

                    // Create our client
                    let client = Arc::new(PushCdnNetwork {
                        client: Client::new(client_config)
                            .await
                            .expect("failed to create client"),
                        #[cfg(feature = "hotshot-testing")]
                        is_paused: Arc::from(AtomicBool::new(false)),
                    });

                    (client.clone(), client)
                })
            }
        })
    }

    /// The PushCDN does not support in-flight message counts
    fn in_flight_message_count(&self) -> Option<usize> {
        None
    }
}

#[async_trait]
impl<TYPES: NodeType> ConnectedNetwork<Message<TYPES>, TYPES::SignatureKey>
    for PushCdnNetwork<TYPES>
{
    /// Pause sending and receiving on the PushCDN network.
    fn pause(&self) {
        #[cfg(feature = "hotshot-testing")]
        self.is_paused.store(true, Ordering::Relaxed);
    }

    /// Resumse sending and receiving on the PushCDN network.
    fn resume(&self) {
        #[cfg(feature = "hotshot-testing")]
        self.is_paused.store(false, Ordering::Relaxed);
    }

    /// The clients form an initial connection when created, so we don't have to wait.
    async fn wait_for_ready(&self) {}

    /// The clients form an initial connection when created, so we can return `true` here
    /// always.
    async fn is_ready(&self) -> bool {
        true
    }

    /// TODO: shut down the networks. Unneeded for testing.
    fn shut_down<'a, 'b>(&'a self) -> BoxSyncFuture<'b, ()>
    where
        'a: 'b,
        Self: 'b,
    {
        boxed_sync(async move {})
    }

    /// Broadcast a message to all members of the quorum.
    ///
    /// # Errors
    /// - If we fail to serialize the message
    /// - If we fail to send the broadcast message.
    async fn broadcast_message<Ver: StaticVersionType>(
        &self,
        message: Message<TYPES>,
        _recipients: BTreeSet<TYPES::SignatureKey>,
        bind_version: Ver,
    ) -> Result<(), NetworkError> {
        self.broadcast_message(message, Topic::Global, bind_version)
            .await
    }

    /// Broadcast a message to all members of the DA committee.
    ///
    /// # Errors
    /// - If we fail to serialize the message
    /// - If we fail to send the broadcast message.
    async fn da_broadcast_message<Ver: StaticVersionType>(
        &self,
        message: Message<TYPES>,
        _recipients: BTreeSet<TYPES::SignatureKey>,
        bind_version: Ver,
    ) -> Result<(), NetworkError> {
        self.broadcast_message(message, Topic::DA, bind_version)
            .await
    }

    /// Send a direct message to a node with a particular key. Does not retry.
    ///
    /// - If we fail to serialize the message
    /// - If we fail to send the direct message
    async fn direct_message<Ver: StaticVersionType>(
        &self,
        message: Message<TYPES>,
        recipient: TYPES::SignatureKey,
        _: Ver,
    ) -> Result<(), NetworkError> {
        // If we're paused, don't send the message
        #[cfg(feature = "hotshot-testing")]
        if self.is_paused.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Bincode the message
        let serialized_message = match Serializer::<Ver>::serialize(&message) {
            Ok(serialized) => serialized,
            Err(e) => {
                warn!("Failed to serialize message: {}", e);
                return Err(NetworkError::FailedToSerialize { source: e });
            }
        };

        // Send the message
        // TODO: check if we need to print this error
        if self
            .client
            .send_direct_message(&WrappedSignatureKey(recipient), serialized_message)
            .await
            .is_err()
        {
            return Err(NetworkError::CouldNotDeliver);
        };

        Ok(())
    }

    /// Receive a message. Is agnostic over `transmit_type`, which has an issue
    /// to be removed anyway.
    ///
    /// # Errors
    /// - If we fail to receive messages. Will trigger a retry automatically.
    async fn recv_msgs(&self) -> Result<Vec<Message<TYPES>>, NetworkError> {
        // Receive a message
        let message = self.client.receive_message().await;

        // If we're paused, receive but don't process messages
        #[cfg(feature = "hotshot-testing")]
        if self.is_paused.load(Ordering::Relaxed) {
            return Ok(vec![]);
        }

        // If it was an error, wait a bit and retry
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                error!("failed to receive message: {error}");
                return Err(NetworkError::PushCdnNetwork {
                    source: PushCdnNetworkError::FailedToReceive,
                });
            }
        };

        // Extract the underlying message
        let (PushCdnMessage::Broadcast(Broadcast { message, topics: _ })
        | PushCdnMessage::Direct(Direct {
            message,
            recipient: _,
        })) = message
        else {
            return Ok(vec![]);
        };

        let message_version = Version::deserialize(&message)
            .map_err(|e| NetworkError::FailedToDeserialize { source: e })?;
        if message_version.0 == VERSION_0_1 {
            let result: Message<TYPES> = Serializer::<Version01>::deserialize(&message)
                .map_err(|e| NetworkError::FailedToDeserialize { source: e })?;

            // Deserialize it
            // Return it
            Ok(vec![result])
        } else {
            Err(NetworkError::FailedToDeserialize {
                source: anyhow::format_err!(
                    "version mismatch, expected {}, got {}",
                    VERSION_0_1,
                    message_version.0
                ),
            })
        }
    }

    /// Do nothing here, as we don't need to look up nodes.
    async fn queue_node_lookup(
        &self,
        _view_number: ViewNumber,
        _pk: TYPES::SignatureKey,
    ) -> Result<(), UnboundedSendError<Option<(ViewNumber, TYPES::SignatureKey)>>> {
        Ok(())
    }

    /// We don't need to poll.
    async fn inject_consensus_info(&self, _event: ConsensusIntentEvent<TYPES::SignatureKey>) {}
}
