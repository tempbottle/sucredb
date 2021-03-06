use std::collections::hash_map::Entry as HMEntry;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, RwLock};
use std::time::Duration;
use std::{io, thread};

use bincode;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use bytes::{BufMut, Bytes, BytesMut};
use linear_map::LinearMap;
use rand::{thread_rng, Rng};

use futures::future::Either;
use futures::sync::mpsc as fmpsc;
use futures::sync::oneshot as foneshot;
use futures::{Future, Sink, Stream};
use tokio_core as tokio;
use tokio_io::codec;
use tokio_io::{io as tokio_io, AsyncRead};

use config::Config;
use database::NodeId;
pub use fabric_msg::*;
use utils::{into_io_error, GenericError, IdHashMap};

// u32(le) payload len + bincode payload
struct FramedBincodeCodec;

impl codec::Decoder for FramedBincodeCodec {
    type Item = FabricMsg;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<Self::Item>> {
        let (consumed, result) = {
            let mut bytes: &[u8] = &*src;
            if let Ok(msg_len) = bytes.read_u32::<LittleEndian>() {
                if bytes.len() >= msg_len as usize {
                    match bincode::deserialize_from(&mut bytes) {
                        Ok(v) => (4 + msg_len as usize, Ok(Some(v))),
                        Err(e) => (0, Err(into_io_error(e))),
                    }
                } else {
                    (0, Ok(None))
                }
            } else {
                (0, Ok(None))
            }
        };
        src.split_to(consumed);
        result
    }
}

impl FramedBincodeCodec {
    fn serialize(item: FabricMsgRef) -> Bytes {
        let item_size = bincode::serialized_size(&item).unwrap();
        let mut dst = BytesMut::with_capacity(item_size as usize + 4);
        dst.put_u32_le(item_size as u32);
        bincode::serialize_into(&mut (&mut dst).writer(), &item).unwrap();
        dst.into()
    }
}

impl codec::Encoder for FramedBincodeCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&mut self, item: Self::Item, dst: &mut BytesMut) -> io::Result<()> {
        dst.reserve(item.len());
        dst.put(&item);
        Ok(())
    }
}

pub type FabricMsgFn = Box<Fn(NodeId, FabricMsg) + Sync + Send>;
pub type FabricConFn = Box<Fn(NodeId) + Sync + Send>;

type SenderChan = fmpsc::UnboundedSender<Bytes>;
type InitType = io::Result<(Arc<SharedContext>, foneshot::Sender<()>)>;

const FABRIC_KEEPALIVE_MS: u64 = 1000;
const FABRIC_RECONNECT_INTERVAL_MS: u64 = 1000;

/// The messaging network that encompasses all nodes of the cluster
/// using the fabric you can send messages (best-effort delivery)
/// to any registered node.
/// Currently each node keeps a connection to every other node. Due to the
/// full-duplex nature of tcp this gives 2 pipes to each server, both are
/// used to make better use of the socket buffers (is this a good idea though?).
/// This also helps parallelism as an eventual big message won't affect
/// the latency as much.
pub struct Fabric {
    context: Arc<SharedContext>,
    loop_thread: Option<(
        foneshot::Sender<()>,
        thread::JoinHandle<Result<(), GenericError>>,
    )>,
}

struct ReaderContext {
    context: Arc<SharedContext>,
    peer: NodeId,
}

struct WriterContext {
    context: Arc<SharedContext>,
    peer: NodeId,
    connection_id: usize,
}

struct SharedContext {
    node: NodeId,
    addr: SocketAddr,
    loop_remote: tokio::reactor::Remote,
    msg_handlers: RwLock<LinearMap<u8, FabricMsgFn>>,
    con_handlers: RwLock<Vec<FabricConFn>>,
    // TODO: unify nodes_addr and connections maps
    nodes_addr: RwLock<IdHashMap<NodeId, SocketAddr>>,
    connections: RwLock<IdHashMap<NodeId, Vec<(usize, SenderChan)>>>,
    connection_gen: AtomicUsize,
}

impl SharedContext {
    fn register_node(&self, peer: NodeId, peer_addr: SocketAddr) -> Option<SocketAddr> {
        self.nodes_addr.write().unwrap().insert(peer, peer_addr)
    }

    fn remove_node(&self, peer: NodeId) -> Option<SocketAddr> {
        self.nodes_addr.write().unwrap().remove(&peer)
    }

    fn register_connection(&self, peer: NodeId, sender: SenderChan) -> usize {
        let connection_id = self.connection_gen.fetch_add(1, Ordering::Relaxed);
        debug!(
            "register_connection peer: {}, id: {:?}",
            peer, connection_id
        );
        let is_new = {
            let mut locked = self.connections.write().unwrap();
            let entry = locked.entry(peer).or_insert_with(Default::default);
            let is_new = entry.is_empty();
            entry.push((connection_id, sender));
            is_new
        };
        if is_new {
            for handler in &*self.con_handlers.read().unwrap() {
                handler(peer);
            }
        }
        connection_id
    }

    fn remove_connection(&self, peer: NodeId, connection_id: usize) {
        debug!("Remove_connection peer: {}, id: {:?}", peer, connection_id);
        let mut locked = self.connections.write().unwrap();
        if let HMEntry::Occupied(mut o) = locked.entry(peer) {
            let p = o
                .get()
                .iter()
                .position(|x| x.0 == connection_id)
                .expect("connection_id not found");
            o.get_mut().swap_remove(p);
            // cleanup entry if empty
            if o.get().is_empty() {
                o.remove();
            }
        } else {
            panic!("Peer not found in connections");
        }
    }
}

impl ReaderContext {
    fn new(context: Arc<SharedContext>, peer: NodeId) -> Self {
        ReaderContext {
            context: context,
            peer: peer,
        }
    }

    fn dispatch(&self, msg: FabricMsg) {
        let msg_type = msg.get_type();
        if let Some(handler) = self
            .context
            .msg_handlers
            .read()
            .unwrap()
            .get(&(msg_type as u8))
        {
            trace!("recv from {:?} {:?}", self.peer, msg);
            handler(self.peer, msg);
        } else {
            error!("No handler for msg type {:?}", msg_type);
        }
    }
}

impl WriterContext {
    fn new(context: Arc<SharedContext>, peer: NodeId, sender: SenderChan) -> Self {
        let connection_id = context.register_connection(peer, sender);
        WriterContext {
            context: context,
            peer: peer,
            connection_id: connection_id,
        }
    }
}

impl Drop for WriterContext {
    fn drop(&mut self) {
        self.context
            .remove_connection(self.peer, self.connection_id);
    }
}

impl Fabric {
    fn listen(
        listener: tokio::net::TcpListener,
        context: Arc<SharedContext>,
        handle: tokio::reactor::Handle,
    ) -> Box<Future<Item = (), Error = ()>> {
        debug!("Starting fabric listener");
        let fut = listener
            .incoming()
            .for_each(move |(socket, addr)| {
                debug!("Accepting connection from {:?}", addr);
                let context_cloned = context.clone();
                handle.spawn(
                    Self::handshake(socket, context_cloned)
                        .and_then(move |(s, peer_id, context)| {
                            Self::steady_connection(s, peer_id, context)
                        })
                        .then(|_| Ok(())),
                );
                Ok(())
            })
            .map_err(|_| ());
        Box::new(fut)
    }

    fn connect(
        expected_node: Option<NodeId>,
        addr: SocketAddr,
        context: Arc<SharedContext>,
        handle: tokio::reactor::Handle,
    ) -> Box<Future<Item = (), Error = ()>> {
        debug!("Connecting to node {:?}: {:?}", expected_node, addr);
        let context1 = context.clone();
        let handle1 = handle.clone();
        let handle2 = handle.clone();

        let fut = tokio::net::TcpStream::connect(&addr, &handle)
            .select2(
                tokio::reactor::Timeout::new(
                    Duration::from_millis(FABRIC_RECONNECT_INTERVAL_MS),
                    &handle,
                ).expect("Can't create connect timeout"),
            )
            .then(|r| match r {
                Ok(Either::A((s, _))) => Ok(s),
                Ok(Either::B(_)) => Err(io::ErrorKind::TimedOut.into()),
                Err(either) => Err(either.split().0),
            })
            .and_then(move |s| Self::handshake(s, context))
            .and_then(move |(s, peer_id, context)| Self::steady_connection(s, peer_id, context))
            .then(move |_| {
                tokio::reactor::Timeout::new(
                    Duration::from_millis(FABRIC_RECONNECT_INTERVAL_MS),
                    &handle1,
                ).expect("Can't create reconnect timeout")
            })
            .and_then(move |_| {
                let node = expected_node.ok_or(io::ErrorKind::NotFound)?;
                let addr_opt = {
                    let locked = context1.nodes_addr.read().unwrap();
                    locked.get(&node).cloned()
                };
                if let Some(addr) = addr_opt {
                    debug!("Reconnecting fabric connection to {:?}", addr);
                    handle2.spawn(Self::connect(
                        expected_node,
                        addr,
                        context1,
                        handle2.clone(),
                    ));
                }
                Ok(())
            });
        Box::new(fut.map_err(|_| ()))
    }

    fn handshake(
        socket: tokio::net::TcpStream,
        context: Arc<SharedContext>,
    ) -> Box<Future<Item = (tokio::net::TcpStream, NodeId, Arc<SharedContext>), Error = io::Error>>
    {
        debug!("Stablished connection with {:?}", socket.peer_addr());
        let _ = socket.set_nodelay(true);
        let _ = socket.set_keepalive(Some(Duration::from_millis(FABRIC_KEEPALIVE_MS)));
        let mut buffer = [0u8; 8];
        (&mut buffer[..])
            .write_u64::<LittleEndian>(context.node)
            .unwrap();
        let fut = tokio_io::write_all(socket, buffer)
            .and_then(|(s, b)| tokio_io::read_exact(s, b))
            .and_then(move |(s, b)| {
                let peer_id = (&b[..]).read_u64::<LittleEndian>().unwrap();
                debug!("Identified connection to node {}", peer_id);
                Ok((s, peer_id, context))
            });

        Box::new(fut)
    }

    fn steady_connection(
        socket: tokio::net::TcpStream,
        peer: NodeId,
        context: Arc<SharedContext>,
    ) -> Box<Future<Item = (), Error = io::Error>> {
        let (socket_rx, socket_tx) = socket.split();
        let socket_tx = codec::FramedWrite::new(socket_tx, FramedBincodeCodec);
        let socket_rx = codec::FramedRead::new(socket_rx, FramedBincodeCodec);
        let (chan_tx, chan_rx) = fmpsc::unbounded();

        let ctx_rx = ReaderContext::new(context.clone(), peer);
        let fut_rx = socket_rx.for_each(move |msg| {
            ctx_rx.dispatch(msg);
            Ok(())
        });

        let ctx_tx = WriterContext::new(context, peer, chan_tx);
        let fut_tx = socket_tx
            .send_all(chan_rx.map_err(|_| io::Error::from(io::ErrorKind::Other)))
            .then(move |r| {
                // hold onto ctx_tx until the stream is done
                drop(ctx_tx);
                r.map(|_| ())
            });

        Box::new(fut_rx.select(fut_tx).map(|_| ()).map_err(|(e, _)| e))
    }

    fn init(
        node: NodeId,
        config: Config,
        handle: tokio::reactor::Handle,
    ) -> Result<Arc<SharedContext>, GenericError> {
        let context = Arc::new(SharedContext {
            node: node,
            addr: config.fabric_addr,
            loop_remote: handle.remote().clone(),
            nodes_addr: Default::default(),
            msg_handlers: Default::default(),
            con_handlers: Default::default(),
            connections: Default::default(),
            connection_gen: Default::default(),
        });

        let listener = tokio::net::TcpListener::bind(&context.addr, &handle)?;
        handle.spawn(Self::listen(listener, context.clone(), handle.clone()));

        Ok(context)
    }

    pub fn node(&self) -> NodeId {
        self.context.node
    }

    pub fn addr(&self) -> SocketAddr {
        self.context.addr
    }

    pub fn new(node: NodeId, config: &Config) -> Result<Self, GenericError> {
        let config = config.clone();
        let (init_tx, init_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name(format!("Fabric:{}", node))
            .spawn(move || {
                let mut core = tokio::reactor::Core::new().unwrap();
                let (completer_tx, completer_rx) = foneshot::channel();
                init_tx.send(Self::init(node, config, core.handle()).map(|c| (c, completer_tx)))?;
                core.run(completer_rx).map_err(From::from)
            })
            .unwrap();
        let (context, completer) = init_rx.recv()??;
        Ok(Fabric {
            context: context,
            loop_thread: Some((completer, thread)),
        })
    }

    pub fn register_msg_handler(&self, msg_type: FabricMsgType, handler: FabricMsgFn) {
        self.context
            .msg_handlers
            .write()
            .unwrap()
            .insert(msg_type as u8, handler);
    }

    pub fn register_con_handler(&self, handler: FabricConFn) {
        self.context.con_handlers.write().unwrap().push(handler);
    }

    pub fn register_seed(&self, addr: SocketAddr) {
        self.start_connect(None, addr)
    }

    pub fn register_node(&self, node: NodeId, addr: SocketAddr) {
        let prev = self.context.register_node(node, addr);
        if prev != Some(addr) {
            self.start_connect(Some(node), addr);
        }
    }

    pub fn remove_node(&self, node: NodeId) {
        self.context.remove_node(node);
    }

    pub fn connections(&self) -> Vec<NodeId> {
        let writers = self.context.connections.read().unwrap();
        writers
            .iter()
            .filter(|&(_, c)| !c.is_empty())
            .map(|(&n, _)| n)
            .collect()
    }

    pub fn set_nodes<I>(&self, it: I)
    where
        I: Iterator<Item = (NodeId, SocketAddr)>,
    {
        let mut nodes = self.context.nodes_addr.write().unwrap();
        let mut x_nodes = nodes.clone();
        for (node, addr) in it {
            if node != self.context.node {
                x_nodes.remove(&node);
                if nodes.insert(node, addr) != Some(addr) {
                    self.start_connect(Some(node), addr);
                }
            }
        }
        for (node, _) in x_nodes {
            nodes.remove(&node);
        }
    }

    fn start_connect(&self, expected_node: Option<NodeId>, addr: SocketAddr) {
        let context = self.context.clone();
        let context_cloned = context.clone();
        context
            .loop_remote
            .spawn(move |h| Self::connect(expected_node, addr, context_cloned, h.clone()));
    }

    // TODO: take msgs as references and buffer serialized bytes instead
    pub fn send_msg<'a, T: Into<FabricMsgRef<'a>>>(
        &'a self,
        node: NodeId,
        msg: T,
    ) -> Result<(), FabricError> {
        let msg = msg.into();
        debug!("send_msg node:{} {:?}", node, msg);
        if node == self.context.node {
            panic!("Can't send message to self");
        }
        if cfg!(test) {
            let droppable = match msg.get_type() {
                FabricMsgType::Crud => false,
                _ => true,
            };
            if droppable {
                let fabric_drop = ::std::env::var("FABRIC_DROP")
                    .ok()
                    .map(|s| s.parse::<f64>().expect("Can't parse FABRIC_DROP"))
                    .unwrap_or(0.0);
                if fabric_drop > 0.0 && thread_rng().gen::<f64>() < fabric_drop {
                    warn!("Fabric msg droped due to FABRIC_DROP: {:?}", msg);
                    return Ok(());
                }
            }
        }

        let serialized_msg = FramedBincodeCodec::serialize(msg);
        let connections = self.context.connections.read().unwrap();
        if let Some(o) = connections.get(&node) {
            if let Some(&(connection_id, ref chan)) = thread_rng().choose::<(_, _)>(o) {
                if let Err(_) = chan.unbounded_send(serialized_msg) {
                    warn!("Can't send to fabric {}-{} chan", node, connection_id,);
                } else {
                    return Ok(());
                }
            } else {
                warn!("DROPING MSG - No channel available for {:?}", node);
            }
        } else {
            warn!("DROPING MSG - No entry for node {:?}", node);
        }

        Err(FabricError::NoRoute)
    }
}

impl Drop for Fabric {
    fn drop(&mut self) {
        warn!("droping fabric");
        if let Some((c, t)) = self.loop_thread.take() {
            let _ = c.send(());
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::Config;
    use env_logger;
    use std::sync::{atomic, Arc};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test() {
        let _ = env_logger::try_init();
        let config1 = Config {
            fabric_addr: "127.0.0.1:6481".parse().unwrap(),
            ..Default::default()
        };
        let config2 = Config {
            fabric_addr: "127.0.0.1:6482".parse().unwrap(),
            ..Default::default()
        };
        let fabric1 = Fabric::new(1, &config1).unwrap();
        let fabric2 = Fabric::new(2, &config2).unwrap();
        fabric1.register_node(2, "127.0.0.1:6482".parse().unwrap());
        fabric2.register_node(1, "127.0.0.1:6481".parse().unwrap());
        thread::sleep(Duration::from_millis(10));

        let counter = Arc::new(atomic::AtomicUsize::new(0));
        let counter_ = counter.clone();
        fabric2.register_msg_handler(
            FabricMsgType::Crud,
            Box::new(move |_, _| {
                counter_.fetch_add(1, atomic::Ordering::Relaxed);
            }),
        );
        for _ in 0..3 {
            fabric1
                .send_msg(
                    2,
                    &MsgRemoteSetAck {
                        cookie: Default::default(),
                        vnode: Default::default(),
                        result: Ok(Vec::new()),
                    },
                )
                .unwrap();
        }
        thread::sleep(Duration::from_millis(10));
        assert_eq!(counter.load(atomic::Ordering::Relaxed), 3);
    }
}
