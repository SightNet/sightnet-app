#[macro_use]
extern crate log;
extern crate pretty_env_logger;

use std::error::Error;
use std::hash::{Hash, Hasher};
use std::time::Duration;
use std::{collections::hash_map::DefaultHasher, env, str::FromStr};
use std::fmt::Formatter;

use async_std::io;
use config::Config;
use futures::{future::Either, prelude::*, select};
use lazy_static::lazy_static;
use libp2p::{
    core::{muxing::StreamMuxerBox, transport::OrTransport, upgrade},
    gossipsub, identity, mdns, noise, quic,
    swarm::NetworkBehaviour,
    swarm::{SwarmBuilder, SwarmEvent},
    tcp, yamux, PeerId, Transport,
    futures::StreamExt,
};
use libp2p::gossipsub::GossipsubEvent;
use libp2p::mdns::MdnsEvent;
use serde::{Deserialize, Serialize};
use sightnet_core::collection::Collection;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use command::{Command, CommandType};

mod command;

#[derive(Debug, Deserialize)]
struct Cfg {
    db_path: String,
    nodes: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    query: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Response {
    receiver: String,
    data: Vec<SearchResult>,
}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    gossipsub: gossipsub::Behaviour,
    mdns: mdns::async_io::Behaviour,
    #[behaviour(ignore)]
    response_sender: UnboundedSender<Response>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SearchResult {
    url: String,
    rank: f32,
}

fn respond(sender: UnboundedSender<Response>, response: Response) {
    tokio::spawn(async move {
        if let Err(e) = sender.send(response) {
            error!("error sending response via channel, {}", e);
        }
    });
}

async fn search(db: &Collection, query: String) -> Vec<SearchResult> {
    db.search(query.as_str(), None, Some(10))
        .iter()
        .map(|x| SearchResult {
            url: "".to_string(),
            rank: x.1,
        })
        .collect()
}

lazy_static! {
    static ref CFG: Cfg = Config::builder()
        .add_source(config::File::with_name("Config.toml"))
        .build()
        .unwrap()
        .try_deserialize::<Cfg>()
        .unwrap();
}

#[async_std::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env::set_var("RUST_LOG", "p2p");

    color_eyre::install()?;
    pretty_env_logger::init();

    let mut db = sightnet_core::file::File::load(CFG.db_path.as_str()).expect("DB file not found!");
    println!("DB documents count: {}", db.len());
    db.commit();

    // Create a random PeerId
    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());
    println!("Local peer id: {local_peer_id}");

    // Set up an encrypted DNS-enabled TCP Transport over the Mplex protocol.
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

    // To content-address message, we can take the hash of message and use it as an ID.
    let message_id_fn = |message: &gossipsub::Message| {
        let mut s = DefaultHasher::new();
        message.data.hash(&mut s);
        gossipsub::MessageId::from(s.finish().to_string())
    };

    // Set a custom gossipsub configuration
    let gossipsub_config = gossipsub::ConfigBuilder::default()
        .heartbeat_interval(Duration::from_secs(10)) // This is set to aid debugging by not cluttering the log space
        .validation_mode(gossipsub::ValidationMode::Strict) // This sets the kind of message validation. The default is Strict (enforce message signing)
        .message_id_fn(message_id_fn) // content-address messages. No two messages of the same content will be propagated.
        .build()
        .expect("Valid config");

    // build a gossipsub network behaviour
    let mut gossipsub = gossipsub::Behaviour::new(
        gossipsub::MessageAuthenticity::Signed(id_keys),
        gossipsub_config,
    )
    .expect("Correct configuration");
    // Create a Gossipsub topic
    let topic = gossipsub::IdentTopic::new("sightnet");
    // subscribes to our topic
    gossipsub.subscribe(&topic)?;

    let (response_sender, mut response_rcv) = unbounded_channel();

    // Create a Swarm to manage peers and events
    let mut swarm = {
        let mdns = mdns::async_io::Behaviour::new(mdns::Config::default(), local_peer_id)?;
        let behaviour = MyBehaviour {
            gossipsub,
            mdns,
            response_sender,
        };
        SwarmBuilder::with_async_std_executor(transport, behaviour, local_peer_id).build()
    };

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines().fuse();

    // Listen on all interfaces and whatever port the OS assigns
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()?)?;

    println!("Enter messages via STDIN and they will be sent to connected peers using Gossipsub");

    // Kick it off
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
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discovered a new peer: {peer_id}");
                        swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
                    for (peer_id, _multiaddr) in list {
                        println!("mDNS discover peer has expired: {peer_id}");
                        swarm.behaviour_mut().gossipsub.remove_explicit_peer(&peer_id);
                    }
                },
                SwarmEvent::Behaviour(MyBehaviourEvent::Gossipsub(gossipsub::Event::Message {
                    propagation_source: peer_id,
                    message_id: id,
                    message,
                })) => {
                        let message = String::from_utf8_lossy(&message.data).to_string();
                        println!("{:#?}", message);
                        let Ok(command) = Command::from_str(message.as_str()) else {continue;};

                        if command.command_type == CommandType::Search {
                            if command.args.len() == 0 {
                                //error
                            }
                            //
                            let results = search(&db, command.args.join(" ")).await;
                    // respond();

                            if let Err(e) = swarm.behaviour_mut().gossipsub
                            .publish(topic.clone(), serde_json::to_string(&results)?.as_bytes()) {
                                println!("Publish error: {e:?}");
                            }
                        }

                        println!("{:#?}", command);
                        // println!(
                        // "Got message: '{}' with id: {id} from peer: {peer_id}",
                        // String::from_utf8_lossy(&message.data),
                    // )
                    },
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Local node is listening on {address}");
                }
                _ => {}
            }
        }
    }
}
