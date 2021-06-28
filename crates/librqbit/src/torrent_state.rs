use std::{
    collections::{HashMap, HashSet},
    fs::File,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use futures::{stream::FuturesUnordered, StreamExt};
use log::warn;
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc::Sender;

use crate::{
    buffers::ByteString,
    chunk_tracker::ChunkTracker,
    files_ops::{check_piece, read_chunk, write_chunk},
    lengths::{ChunkInfo, Lengths, ValidPieceIndex},
    peer_binary_protocol::{Handshake, Message, MessageOwned, Piece},
    peer_state::{LivePeerState, PeerState},
    torrent_metainfo::TorrentMetaV1Owned,
    type_aliases::{PeerHandle, BF},
};

#[derive(Debug, Hash, PartialEq, Eq)]
pub struct InflightRequest {
    pub piece: ValidPieceIndex,
    pub chunk: u32,
}

impl From<&ChunkInfo> for InflightRequest {
    fn from(c: &ChunkInfo) -> Self {
        Self {
            piece: c.piece_index,
            chunk: c.chunk_index,
        }
    }
}

#[derive(Default)]
pub struct PeerStates {
    states: HashMap<PeerHandle, PeerState>,
    seen_peers: HashSet<SocketAddr>,
    inflight_pieces: HashSet<ValidPieceIndex>,
    tx: HashMap<PeerHandle, Arc<tokio::sync::mpsc::Sender<MessageOwned>>>,
}

#[derive(Debug, Default)]
pub struct AggregatePeerStats {
    pub connecting: usize,
    pub live: usize,
}

impl PeerStates {
    pub fn stats(&self) -> AggregatePeerStats {
        self.states
            .values()
            .fold(AggregatePeerStats::default(), |mut s, p| {
                match p {
                    PeerState::Connecting(_) => s.connecting += 1,
                    PeerState::Live(_) => s.live += 1,
                };
                s
            })
    }
    pub fn add_if_not_seen(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        if self.seen_peers.contains(&addr) {
            return None;
        }
        let handle = self.add(addr, tx)?;
        self.seen_peers.insert(addr);
        Some(handle)
    }
    pub fn get_live(&self, handle: PeerHandle) -> Option<&LivePeerState> {
        if let PeerState::Live(ref l) = self.states.get(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn get_live_mut(&mut self, handle: PeerHandle) -> Option<&mut LivePeerState> {
        if let PeerState::Live(ref mut l) = self.states.get_mut(&handle)? {
            return Some(l);
        }
        None
    }
    pub fn try_get_live_mut(&mut self, handle: PeerHandle) -> anyhow::Result<&mut LivePeerState> {
        self.get_live_mut(handle)
            .ok_or_else(|| anyhow::anyhow!("peer dropped"))
    }
    pub fn add(
        &mut self,
        addr: SocketAddr,
        tx: tokio::sync::mpsc::Sender<MessageOwned>,
    ) -> Option<PeerHandle> {
        let handle = addr;
        if self.states.contains_key(&addr) {
            return None;
        }
        self.states.insert(handle, PeerState::Connecting(addr));
        self.tx.insert(handle, Arc::new(tx));
        Some(handle)
    }
    pub fn drop_peer(&mut self, handle: PeerHandle) -> Option<PeerState> {
        let result = self.states.remove(&handle);
        self.tx.remove(&handle);
        result
    }
    pub fn mark_i_am_choked(&mut self, handle: PeerHandle, is_choked: bool) -> Option<bool> {
        let live = self.get_live_mut(handle)?;
        let prev = live.i_am_choked;
        live.i_am_choked = is_choked;
        Some(prev)
    }
    pub fn update_bitfield_from_vec(
        &mut self,
        handle: PeerHandle,
        bitfield: Vec<u8>,
    ) -> Option<Option<BF>> {
        let live = self.get_live_mut(handle)?;
        let bitfield = BF::from_vec(bitfield);
        let prev = live.bitfield.take();
        live.bitfield = Some(bitfield);
        Some(prev)
    }
    pub fn clone_tx(&self, handle: PeerHandle) -> Option<Arc<Sender<MessageOwned>>> {
        Some(self.tx.get(&handle)?.clone())
    }
    pub fn remove_inflight_piece(&mut self, piece: ValidPieceIndex) -> bool {
        self.inflight_pieces.remove(&piece)
    }
}

pub struct TorrentStateLocked {
    pub peers: PeerStates,
    pub chunks: ChunkTracker,
}

pub struct AtomicStats {
    pub have: AtomicU64,
    pub downloaded_and_checked: AtomicU64,
    pub uploaded: AtomicU64,
    pub fetched_bytes: AtomicU64,
}

pub struct TorrentState {
    pub torrent: TorrentMetaV1Owned,
    pub locked: Arc<RwLock<TorrentStateLocked>>,
    pub files: Vec<Arc<Mutex<File>>>,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub lengths: Lengths,
    pub needed: u64,
    pub stats: AtomicStats,
}

impl TorrentState {
    pub fn check_piece_blocking(
        &self,
        who_sent: PeerHandle,
        piece_index: ValidPieceIndex,
        last_received_chunk: &ChunkInfo,
    ) -> anyhow::Result<bool> {
        check_piece(
            &self.torrent,
            &self.files,
            &self.lengths,
            who_sent,
            piece_index,
            last_received_chunk,
        )
    }

    pub fn read_chunk_blocking(
        &self,
        who_sent: PeerHandle,
        chunk_info: ChunkInfo,
    ) -> anyhow::Result<Vec<u8>> {
        read_chunk(
            &self.torrent,
            &self.files,
            &self.lengths,
            who_sent,
            chunk_info,
        )
    }

    pub fn write_chunk_blocking(
        &self,
        who_sent: PeerHandle,
        data: &Piece<ByteString>,
        chunk_info: &ChunkInfo,
    ) -> anyhow::Result<()> {
        write_chunk(
            &self.torrent,
            &self.files,
            &self.lengths,
            who_sent,
            data,
            chunk_info,
        )
    }

    pub fn get_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        let g = self.locked.read();
        let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
        for n in g.chunks.get_needed_pieces().iter_ones() {
            if bf.get(n).map(|v| *v) == Some(true) {
                // in theory it should be safe without validation, but whatever.
                return self.lengths.validate_piece_index(n as u32);
            }
        }
        None
    }

    pub fn am_i_choked(&self, peer_handle: PeerHandle) -> Option<bool> {
        self.locked
            .read()
            .peers
            .get_live(peer_handle)
            .map(|l| l.i_am_choked)
    }

    pub fn reserve_next_needed_piece(&self, peer_handle: PeerHandle) -> Option<ValidPieceIndex> {
        if self.am_i_choked(peer_handle)? {
            warn!("we are choked by {}, can't reserve next piece", peer_handle);
            return None;
        }
        let mut g = self.locked.write();
        let n = {
            let mut n_opt = None;
            let bf = g.peers.get_live(peer_handle)?.bitfield.as_ref()?;
            for n in g.chunks.get_needed_pieces().iter_ones() {
                if bf.get(n).map(|v| *v) == Some(true) {
                    n_opt = Some(n);
                    break;
                }
            }

            self.lengths.validate_piece_index(n_opt? as u32)?
        };
        g.peers.inflight_pieces.insert(n);
        g.chunks.reserve_needed_piece(n);
        Some(n)
    }

    pub fn am_i_interested_in_peer(&self, handle: PeerHandle) -> bool {
        self.get_next_needed_piece(handle).is_some()
    }

    pub fn try_steal_piece(&self, handle: PeerHandle) -> Option<ValidPieceIndex> {
        let mut rng = rand::thread_rng();
        use rand::seq::IteratorRandom;
        let g = self.locked.read();
        let pl = g.peers.get_live(handle)?;
        g.peers
            .inflight_pieces
            .iter()
            .filter(|p| !pl.inflight_requests.iter().any(|req| req.piece == **p))
            .choose(&mut rng)
            .copied()
    }

    pub fn set_peer_live(&self, handle: PeerHandle, h: Handshake) {
        let mut g = self.locked.write();
        match g.peers.states.get_mut(&handle) {
            Some(s @ &mut PeerState::Connecting(_)) => {
                *s = PeerState::Live(LivePeerState::new(h.peer_id));
            }
            _ => {
                warn!("peer {} was in wrong state", handle);
            }
        }
    }

    pub fn drop_peer(&self, handle: PeerHandle) -> bool {
        let mut g = self.locked.write();
        let peer = match g.peers.drop_peer(handle) {
            Some(peer) => peer,
            None => return false,
        };
        match peer {
            PeerState::Connecting(_) => {}
            PeerState::Live(l) => {
                for req in l.inflight_requests {
                    g.chunks.mark_chunk_request_cancelled(req.piece, req.chunk);
                }
            }
        }
        true
    }

    pub fn get_uploaded(&self) -> u64 {
        self.stats.uploaded.load(Ordering::Relaxed)
    }
    pub fn get_downloaded(&self) -> u64 {
        self.stats.downloaded_and_checked.load(Ordering::Relaxed)
    }

    pub fn get_left_to_download(&self) -> u64 {
        self.needed - self.get_downloaded()
    }

    // TODO: this is a task per chunk, not good
    pub async fn task_transmit_haves(&self, index: u32) -> anyhow::Result<()> {
        let mut unordered = FuturesUnordered::new();

        for weak in self
            .locked
            .read()
            .peers
            .tx
            .values()
            .map(|v| Arc::downgrade(v))
        {
            unordered.push(async move {
                if let Some(tx) = weak.upgrade() {
                    if tx.send(Message::Have(index)).await.is_err() {
                        // whatever
                    }
                }
            });
        }

        while unordered.next().await.is_some() {}
        Ok(())
    }
}
