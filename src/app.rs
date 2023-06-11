use std::{collections::hash_map::DefaultHasher, env, str::FromStr};
use std::error::Error;
use std::fmt::Formatter;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use async_std::io;
use config::Config;
use futures::{future::Either, prelude::*, select};
use lazy_static::lazy_static;
use libp2p::{
    core::{muxing::StreamMuxerBox, transport::OrTransport, upgrade},
    futures::StreamExt, gossipsub, identity, mdns, noise,
    PeerId,
    quic,
    swarm::{SwarmBuilder, SwarmEvent}, swarm::NetworkBehaviour, tcp, Transport,
    yamux,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::unbounded_channel;

#[derive(Debug, Deserialize)]
pub struct Cfg {
    pub db_url: String,
    pub db_name: String,
    pub bootstrap_nodes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchRequest {
    query: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchResponse {
    receiver: String,
    results: Vec<SearchResult>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchResult {
    url: String,
    rank: f32,
}

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "MyEvent")]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::async_io::Behaviour,
}

enum MyEvent {
    Gossipsub(gossipsub::Event),
    Mdns(mdns::Event),
}

impl From<gossipsub::Event> for MyEvent {
    fn from(event: gossipsub::Event) -> Self {
        Self::Gossipsub(event)
    }
}

impl From<mdns::Event> for MyEvent {
    fn from(event: mdns::Event) -> Self {
        Self::Mdns(event)
    }
}

lazy_static! {
    static ref CFG: Cfg = Config::builder()
        .add_source(config::File::with_name("Config.toml"))
        .build()
        .unwrap()
        .try_deserialize::<Cfg>()
        .unwrap();
}

async fn search(query: String) -> Vec<SearchResult> {
    let url = format!("{}/{}/search?q={}", CFG.db_url, CFG.db_name, query);
    let res: serde_json::Value = ureq::get(url.as_str()).call()
        .expect("Response").into_json().expect("Valid json");
    let error = res.get("error");

    if let Some(error) = error {
        panic!("Error while searching in db: {}", error)
    }

    let results = res.as_array().expect("Valid json array of results");
    let mut search_results = vec![];

    for result in results {
        search_results.push(SearchResult {
            url: result["url"].to_string(),
            rank: result["rank"].as_f64().expect("Valid rank") as f32,
        });
    }

    search_results
}

pub async fn start() -> Result<(), Box<dyn Error>> {
    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());

    let tcp_transport = tcp::async_io::Transport::new(tcp::Config::default().nodelay(true))
        .upgrade(upgrade::Version::V1)
        .authenticate(
            noise::NoiseAuthenticated::xx(&id_keys).expect("signing libp2p-noise static keypair"),
        )
        .multiplex(yamux::YamuxConfig::default())
        .timeout(Duration::from_secs(20))
        .boxed();
    let quic_transport = quic::async_std::Transport::new(quic::Config::new(&id_keys));
    let transport = OrTransport::new(quic_transport, tcp_transport)
        .map(|either_output, _| match either_output {
            Either::Left((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
            Either::Right((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
        })
        .boxed();

    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10))
        .validation_mode(gossipsub::ValidationMode::Strict)
        .message_id_fn(message_id_fn)
        .build()
        .expect("Valid config");

    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
    )
        .expect("Correct configuration");
    let topic = gossipsub::IdentTopic::new("sightnet");
    gossipsub.subscribe(&topic)?;

    // let (response_sender, mut response_rcv) = unbounded_channel();

    let mut swarm = {
        let mdns = mdns::async_io::Behaviour::new(mdns::Config::default(), local_peer_id)?;
        let behaviour = MyBehaviour {
            gossipsub,
            mdns,
        };
        SwarmBuilder::with_async_std_executor(transport, behaviour, local_peer_id).build()
    };

    let mut stdin = io::BufReader::new(io::stdin()).lines().fuse();

    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    loop {
        select! {
            line = stdin.select_next_some() => {
                if let Err(e) = swarm
                    .behaviour_mut().gossipsub
                    .publish(topic.clone(), line.expect("Stdin not to close").as_bytes()) {
                    println!("Publish error: {e:?}");
                }
            },
            event = swarm.select_next_some() => match event {
                SwarmEvent::Behaviour(MyEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discovered a new peer: {peer_id}");
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => {
                    let message = String::from_utf8_lossy(&message.data).to_string();
                    println!("{:#?}", message);
                    let results = search(message).await;

                    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), serde_json::to_string(&results)?.as_bytes()) {
                        println!("Publish error: {e:?}");
                    }
                },
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                }
                _ => {}
            }
        }
    }
}
