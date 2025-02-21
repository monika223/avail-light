use super::{
	event_loop::ConnectionEstablishedInfo, is_global, is_multiaddr_global, Command, CommandSender,
	EventLoopEntries, MultiAddressInfo, PeerInfo, QueryChannel, SendableCommand,
};
use color_eyre::{
	eyre::{eyre, WrapErr},
	Report, Result,
};
use futures::future::join_all;
use kate_recovery::{
	config,
	data::Cell,
	matrix::{Dimensions, Position, RowIndex},
};
use libp2p::{
	kad::{store::RecordStore, Mode, PeerRecord, Quorum, Record, RecordKey},
	swarm::dial_opts::DialOpts,
	Multiaddr, PeerId,
};
use std::time::{Duration, Instant};
use sysinfo::System;
use tokio::sync::oneshot;
use tracing::{debug, info, trace};

#[derive(Clone)]
pub struct Client {
	command_sender: CommandSender,
	/// Number of cells to fetch in parallel
	dht_parallelization_limit: usize,
	/// Cell time to live in DHT (in seconds)
	ttl: u64,
}

struct DHTCell(Cell);

impl DHTCell {
	fn reference(&self, block: u32) -> String {
		self.0.reference(block)
	}

	fn dht_record(&self, block: u32, ttl: u64) -> Record {
		Record {
			key: self.0.reference(block).as_bytes().to_vec().into(),
			value: self.0.content.to_vec(),
			publisher: None,
			expires: Instant::now().checked_add(Duration::from_secs(ttl)),
		}
	}
}
struct DHTRow((RowIndex, Vec<u8>));

impl DHTRow {
	fn reference(&self, block: u32) -> String {
		self.0 .0.reference(block)
	}

	fn dht_record(&self, block: u32, ttl: u64) -> Record {
		Record {
			key: self.0 .0.reference(block).as_bytes().to_vec().into(),
			value: self.0 .1.clone(),
			publisher: None,
			expires: Instant::now().checked_add(Duration::from_secs(ttl)),
		}
	}
}

#[derive(Debug)]
pub struct BlockStat {
	pub total_count: usize,
	pub remaining_counter: usize,
	pub success_counter: usize,
	pub error_counter: usize,
	pub time_stat: u64,
}

impl BlockStat {
	pub fn increase_block_stat_counters(&mut self, cell_number: usize) {
		self.total_count += cell_number;
		self.remaining_counter += cell_number;
	}
}

struct PruneExpiredRecords {
	#[allow(dead_code)]
	now: Instant,
	response_sender: Option<oneshot::Sender<Result<usize>>>,
}

#[cfg(not(feature = "kademlia-rocksdb"))]
impl Command for PruneExpiredRecords {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<(), Report> {
		let store = entries.behavior_mut().kademlia.store_mut();

		let before = store.records().count();
		store.retain(|_, record| !record.is_expired(self.now));
		let after = store.records().count();

		self.response_sender
			.take()
			.unwrap()
			.send(Ok(before - after))
			.expect("PruneExpiredRecords receiver dropped");

		Ok(())
	}

	fn abort(&mut self, _: Report) {}
}

#[cfg(feature = "kademlia-rocksdb")]
impl Command for PruneExpiredRecords {
	fn run(&mut self, _: EventLoopEntries) -> Result<(), Report> {
		// Skip iterating all records from RocksDB, since TTL will be handled during compaction phase
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(0))
			.expect("PruneExpiredRecords receiver dropped");

		Ok(())
	}

	fn abort(&mut self, _: Report) {}
}

struct StartListening {
	addr: Multiaddr,
	response_sender: Option<oneshot::Sender<Result<()>>>,
}

impl Command for StartListening {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		_ = entries.swarm().listen_on(self.addr.clone())?;

		// send result back
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(()))
			.expect("StartListening receiver dropped");
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("StartListening receiver dropped");
	}
}

struct AddAddress {
	peer_id: PeerId,
	peer_addr: Multiaddr,
}

impl Command for AddAddress {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		_ = entries
			.behavior_mut()
			.kademlia
			.add_address(&self.peer_id, self.peer_addr.clone());

		Ok(())
	}

	fn abort(&mut self, _error: Report) {}
}

struct Bootstrap {
	response_sender: Option<oneshot::Sender<Result<()>>>,
}

impl Command for Bootstrap {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		let query_id = entries.behavior_mut().kademlia.bootstrap()?;

		// insert response channel into KAD Queries pending map
		let response_sender = self.response_sender.take().unwrap();
		entries.insert_query(query_id, super::QueryChannel::Bootstrap(response_sender));
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("Bootstrap receiver dropped");
	}
}

struct GetKadRecord {
	key: RecordKey,
	response_sender: Option<oneshot::Sender<Result<PeerRecord>>>,
}

impl Command for GetKadRecord {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		let query_id = entries.behavior_mut().kademlia.get_record(self.key.clone());

		// insert response channel into KAD Queries pending map
		let response_sender = self.response_sender.take().unwrap();
		entries.insert_query(query_id, super::QueryChannel::GetRecord(response_sender));
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("GetKadRecord receiver dropped");
	}
}

struct PutKadRecord {
	records: Vec<Record>,
	quorum: Quorum,
	block_num: u32,
}

// `active_blocks` is a list of cell counts for each block we monitor for PUT op. results
impl Command for PutKadRecord {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		entries
			.active_blocks
			.entry(self.block_num)
			// Increase the total cell count we monitor if the block entry already exists
			.and_modify(|block| block.increase_block_stat_counters(self.records.len()))
			// Initiate counting for the new block if the block doesn't exist
			.or_insert(BlockStat {
				total_count: self.records.len(),
				remaining_counter: self.records.len(),
				success_counter: 0,
				error_counter: 0,
				time_stat: 0,
			});

		for record in self.records.clone() {
			let query_id = entries
				.behavior_mut()
				.kademlia
				.put_record(record, self.quorum)
				.expect("Unable to perform Kademlia PUT operation.");
			entries.insert_query(query_id, QueryChannel::PutRecord);
		}
		Ok(())
	}

	fn abort(&mut self, _: Report) {}
}

struct CountKademliaPeers {
	response_sender: Option<oneshot::Sender<Result<(usize, usize)>>>,
}

impl Command for CountKademliaPeers {
	fn run(&mut self, entries: EventLoopEntries) -> Result<()> {
		let mut total_peers: usize = 0;
		let mut peers_with_non_pvt_addr: usize = 0;
		for bucket in entries.swarm.behaviour_mut().kademlia.kbuckets() {
			for item in bucket.iter() {
				for address in item.node.value.iter() {
					if is_multiaddr_global(address) {
						peers_with_non_pvt_addr += 1;
						// We just need to hit the first external address
						break;
					}
				}
				total_peers += 1;
			}
		}
		self.response_sender
			.take()
			.unwrap()
			.send(Ok((total_peers, peers_with_non_pvt_addr)))
			.expect("CountKademliaPeers receiver dropped");
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("CountDHTPeers receiver dropped");
	}
}

struct GetLocalInfo {
	response_sender: Option<oneshot::Sender<Result<PeerInfo>>>,
}

impl Command for GetLocalInfo {
	fn run(&mut self, entries: EventLoopEntries) -> Result<(), Report> {
		let public_listeners: Vec<String> = entries
			.swarm
			.external_addresses()
			.filter(|multiaddr| {
				multiaddr.iter().any(
					|protocol| matches!(protocol, libp2p::multiaddr::Protocol::Ip4(ip) if is_global(ip)),
				)
			})
			.map(ToString::to_string)
			.collect();

		self.response_sender
			.take()
			.unwrap()
			.send(Ok(PeerInfo {
				peer_id: entries.peer_id().to_string(),
				operation_mode: entries.kad_mode.to_string(),
				peer_multiaddr: None,
				local_listeners: entries.listeners(),
				external_listeners: entries.external_address(),
				public_listeners,
			}))
			.expect("GetLocalInfo receiver dropped");

		Ok(())
	}

	fn abort(&mut self, error: Report) {
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("GetLocalInfo receiver dropped");
	}
}

struct GetExternalPeerInfo {
	peer_id: PeerId,
	response_sender: Option<oneshot::Sender<Result<MultiAddressInfo>>>,
}

impl Command for GetExternalPeerInfo {
	fn run(&mut self, entries: EventLoopEntries) -> Result<(), Report> {
		let mut multiaddresses: Vec<String> = Vec::new();

		for bucket in entries.swarm.behaviour_mut().kademlia.kbuckets() {
			for item in bucket.iter() {
				if *item.node.key.preimage() == self.peer_id {
					for addr in item.node.value.iter() {
						multiaddresses.push(addr.to_string());
					}
				}
			}
		}

		self.response_sender
			.take()
			.unwrap()
			.send(Ok(MultiAddressInfo {
				multiaddresses,
				peer_id: self.peer_id.to_string(),
			}))
			.expect("GetExternalPeerInfo receiver dropped");

		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("GetExternalPeerInfo receiver dropped");
	}
}

struct ListConnectedPeers {
	response_sender: Option<oneshot::Sender<Result<Vec<String>>>>,
}

impl Command for ListConnectedPeers {
	fn run(&mut self, entries: EventLoopEntries) -> Result<()> {
		let connected_peer_list = entries
			.swarm
			.connected_peers()
			.map(|peer_id| peer_id.to_string())
			.collect::<Vec<_>>();

		// send result back
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(connected_peer_list))
			.expect("CountDHTPeers receiver dropped");
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("CountDHTPeers receiver dropped");
	}
}

struct ReconfigureKademliaMode {
	response_sender: Option<oneshot::Sender<Result<Mode>>>,
	memory_gb_threshold: f64,
	cpus_threshold: usize,
}

impl Command for ReconfigureKademliaMode {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		if matches!(entries.kad_mode, Mode::Client) && !entries.external_address().is_empty() {
			const BYTES_IN_GB: usize = 1024 * 1024 * 1024;

			let system = System::new_all();
			let memory_gb = system.total_memory() as f64 / BYTES_IN_GB as f64;
			let cpus = system.cpus().len();
			trace!("Total memory: {memory_gb} GB, CPU core count: {cpus}");

			if memory_gb > self.memory_gb_threshold && cpus > self.cpus_threshold {
				info!("Switching Kademlia mode to server!");
				entries.behavior_mut().kademlia.set_mode(Some(Mode::Server));
				*entries.kad_mode = Mode::Server;
			}
		} else if matches!(entries.kad_mode, Mode::Server) && entries.external_address().is_empty()
		{
			info!("Peer is not externally reachable, switching to client mode.");
			entries.behavior_mut().kademlia.set_mode(Some(Mode::Client));
			*entries.kad_mode = Mode::Client;
		}

		// send result back
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(*entries.kad_mode))
			.expect("ReconfigureKademliaMode receiver dropped");
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("ReconfigureKademliaMode receiver dropped");
	}
}

struct ReduceKademliaMapSize {
	response_sender: Option<oneshot::Sender<Result<()>>>,
}

impl Command for ReduceKademliaMapSize {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		entries.behavior_mut().kademlia.store_mut().shrink_hashmap();

		// send result back
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(()))
			.expect("ReduceKademliaMapSize receiver dropped");
		Ok(())
	}

	fn abort(&mut self, _: Report) {
		// theres should be no errors from running this Command
		debug!("No possible errors for ReduceKademliaMapSize");
	}
}

struct GetKademliaMapSize {
	response_sender: Option<oneshot::Sender<Result<usize>>>,
}

impl Command for GetKademliaMapSize {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<(), Report> {
		let size = entries
			.behavior_mut()
			.kademlia
			.store_mut()
			.records()
			.count();

		self.response_sender
			.take()
			.unwrap()
			.send(Ok(size))
			.expect("GetKademliaMapSize receiver dropped");
		Ok(())
	}

	fn abort(&mut self, _: Report) {
		// theres should be no errors from running this Command
		debug!("No possible errors for GetKademliaMapSize");
	}
}

struct DialPeer {
	peer_id: PeerId,
	peer_address: Vec<Multiaddr>,
	response_sender: Option<oneshot::Sender<Result<ConnectionEstablishedInfo>>>,
}

impl Command for DialPeer {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		let opts = DialOpts::peer_id(self.peer_id)
			.addresses(self.peer_address.clone())
			.build();

		entries.swarm().dial(opts)?;

		// insert response channel into Swarm Events pending map
		entries.insert_swarm_event(self.peer_id, self.response_sender.take().unwrap());
		Ok(())
	}

	fn abort(&mut self, error: Report) {
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Err(error))
			.expect("DialPeer receiver dropped");
	}
}

struct AddAutonatServer {
	peer_id: PeerId,
	address: Multiaddr,
	response_sender: Option<oneshot::Sender<Result<()>>>,
}

impl Command for AddAutonatServer {
	fn run(&mut self, mut entries: EventLoopEntries) -> Result<()> {
		entries
			.behavior_mut()
			.auto_nat
			.add_server(self.peer_id, Some(self.address.clone()));

		// send result back
		// TODO: consider what to do if this results with None
		self.response_sender
			.take()
			.unwrap()
			.send(Ok(()))
			.expect("AddAutonatServer receiver dropped");
		Ok(())
	}

	fn abort(&mut self, _: Report) {
		// theres should be no errors from running this Command
		debug!("No possible errors for AddAutonatServer command");
	}
}

impl Client {
	pub fn new(sender: CommandSender, dht_parallelization_limit: usize, ttl: u64) -> Self {
		Self {
			command_sender: sender,
			dht_parallelization_limit,
			ttl,
		}
	}

	async fn execute_sync<F, T>(&self, command_with_sender: F) -> Result<T>
	where
		F: FnOnce(oneshot::Sender<Result<T>>) -> SendableCommand,
	{
		let (response_sender, response_receiver) = oneshot::channel();
		let command = command_with_sender(response_sender);
		self.command_sender
			.send(command)
			.wrap_err("receiver should not be dropped")?;
		response_receiver
			.await
			.wrap_err("sender should not be dropped")?
	}

	pub async fn start_listening(&self, addr: Multiaddr) -> Result<()> {
		self.execute_sync(|response_sender| {
			Box::new(StartListening {
				addr,
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn add_address(&self, peer_id: PeerId, peer_addr: Multiaddr) -> Result<()> {
		self.command_sender
			.send(Box::new(AddAddress { peer_id, peer_addr }))
			.context("failed to add address to the routing table")
	}

	pub async fn dial_peer(
		&self,
		peer_id: PeerId,
		peer_address: Vec<Multiaddr>,
	) -> Result<ConnectionEstablishedInfo> {
		self.execute_sync(|response_sender| {
			Box::new(DialPeer {
				peer_id,
				peer_address,
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn bootstrap(&self) -> Result<()> {
		self.execute_sync(|response_sender| {
			Box::new(Bootstrap {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn add_autonat_server(&self, peer_id: PeerId, address: Multiaddr) -> Result<()> {
		self.execute_sync(|response_sender| {
			Box::new(AddAutonatServer {
				peer_id,
				address,
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn bootstrap_on_startup(&self, nodes: Vec<(PeerId, Multiaddr)>) -> Result<()> {
		for (peer, addr) in nodes {
			self.dial_peer(peer, vec![addr.clone()])
				.await
				.wrap_err("Dialing Bootstrap peer failed.")?;
			self.add_address(peer, addr.clone()).await?;

			self.add_autonat_server(peer, addr).await?;
		}
		self.bootstrap().await
	}

	async fn get_kad_record(&self, key: RecordKey) -> Result<PeerRecord> {
		self.execute_sync(|response_sender| {
			Box::new(GetKadRecord {
				key,
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	async fn put_kad_record(
		&self,
		records: Vec<Record>,
		quorum: Quorum,
		block_num: u32,
	) -> Result<()> {
		self.command_sender
			.send(Box::new(PutKadRecord {
				records,
				quorum,
				block_num,
			}))
			.context("receiver should not be dropped")
	}

	pub async fn count_dht_entries(&self) -> Result<(usize, usize)> {
		self.execute_sync(|response_sender| {
			Box::new(CountKademliaPeers {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn list_connected_peers(&self) -> Result<Vec<String>> {
		self.execute_sync(|response_sender| {
			Box::new(ListConnectedPeers {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn reconfigure_kademlia_mode(
		&self,
		memory_gb_threshold: f64,
		cpus_threshold: usize,
	) -> Result<Mode> {
		self.execute_sync(|response_sender| {
			Box::new(ReconfigureKademliaMode {
				response_sender: Some(response_sender),
				memory_gb_threshold,
				cpus_threshold,
			})
		})
		.await
	}

	pub async fn get_local_info(&self) -> Result<PeerInfo> {
		self.execute_sync(|response_sender| {
			Box::new(GetLocalInfo {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn get_external_peer_info(&self, peer_id: PeerId) -> Result<MultiAddressInfo> {
		self.execute_sync(|response_sender| {
			Box::new(GetExternalPeerInfo {
				peer_id,
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	// Reduces the size of Kademlias underlying hashmap
	pub async fn shrink_kademlia_map(&self) -> Result<()> {
		self.execute_sync(|response_sender| {
			Box::new(ReduceKademliaMapSize {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn get_kademlia_map_size(&self) -> Result<usize> {
		self.execute_sync(|response_sender| {
			Box::new(GetKademliaMapSize {
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	pub async fn prune_expired_records(&self) -> Result<usize> {
		self.execute_sync(|response_sender| {
			Box::new(PruneExpiredRecords {
				now: Instant::now(),
				response_sender: Some(response_sender),
			})
		})
		.await
	}

	// Since callers ignores DHT errors, debug logs are used to observe DHT behavior.
	// Return type assumes that cell is not found in case when error is present.
	async fn fetch_cell_from_dht(&self, block_number: u32, position: Position) -> Option<Cell> {
		let reference = position.reference(block_number);
		let record_key = RecordKey::from(reference.as_bytes().to_vec());

		trace!("Getting DHT record for reference {}", reference);

		match self.get_kad_record(record_key).await {
			Ok(peer_record) => {
				trace!("Fetched cell {reference} from the DHT");

				let try_content: Result<[u8; config::COMMITMENT_SIZE + config::CHUNK_SIZE], _> =
					peer_record.record.value.try_into();

				let Ok(content) = try_content else {
					debug!("Cannot convert cell {reference} into 80 bytes");
					return None;
				};

				Some(Cell { position, content })
			},
			Err(error) => {
				trace!("Cell {reference} not found in the DHT: {error}");
				None
			},
		}
	}

	async fn fetch_row_from_dht(
		&self,
		block_number: u32,
		row_index: u32,
	) -> Option<(u32, Vec<u8>)> {
		let row_index = RowIndex(row_index);
		let reference = row_index.reference(block_number);
		let record_key = RecordKey::from(reference.as_bytes().to_vec());

		trace!("Getting DHT record for reference {}", reference);

		match self.get_kad_record(record_key).await {
			Ok(peer_record) => Some((row_index.0, peer_record.record.value)),
			Err(error) => {
				debug!("Row {reference} not found in the DHT: {error}");
				None
			},
		}
	}

	/// Fetches cells from DHT.
	/// Returns fetched cells and unfetched positions (so we can try RPC fetch).
	///
	/// # Arguments
	///
	/// * `block_number` - Block number
	/// * `positions` - Cell positions to fetch
	pub async fn fetch_cells_from_dht(
		&self,
		block_number: u32,
		positions: &[Position],
	) -> (Vec<Cell>, Vec<Position>) {
		let mut cells = Vec::<Option<Cell>>::with_capacity(positions.len());

		for positions in positions.chunks(self.dht_parallelization_limit) {
			let fetch = |&position| self.fetch_cell_from_dht(block_number, position);
			let results = join_all(positions.iter().map(fetch)).await;
			cells.extend(results.into_iter().collect::<Vec<_>>());
		}

		let unfetched = cells
			.iter()
			.zip(positions)
			.filter(|(cell, _)| cell.is_none())
			.map(|(_, &position)| position)
			.collect::<Vec<_>>();

		let fetched = cells.into_iter().flatten().collect();

		(fetched, unfetched)
	}

	/// Fetches rows from DHT.
	/// Returns fetched rows and unfetched row indexes (so we can try RPC fetch).
	///
	/// # Arguments
	///
	/// * `block_number` - Block number
	/// * `rows` - Row indexes to fetch
	pub async fn fetch_rows_from_dht(
		&self,
		block_number: u32,
		dimensions: Dimensions,
		row_indexes: &[u32],
	) -> Vec<Option<Vec<u8>>> {
		let mut rows = vec![None; dimensions.extended_rows() as usize];
		for row_indexes in row_indexes.chunks(self.dht_parallelization_limit) {
			let fetch = |row| self.fetch_row_from_dht(block_number, row);
			let fetched_rows = join_all(row_indexes.iter().cloned().map(fetch)).await;
			for (row_index, row) in fetched_rows.into_iter().flatten() {
				rows[row_index as usize] = Some(row);
			}
		}
		rows
	}

	async fn insert_into_dht(&self, records: Vec<(String, Record)>, block_num: u32) -> Result<()> {
		if records.is_empty() {
			return Err(eyre!("Cant send empty record list."));
		}
		self.put_kad_record(
			records.into_iter().map(|e| e.1).collect(),
			Quorum::One,
			block_num,
		)
		.await
	}

	/// Inserts cells into the DHT.
	/// There is no rollback, and errors will be logged and skipped,
	/// which means that we cannot rely on error logs as alert mechanism.
	/// Returns the success rate of the PUT operations measured by dividing
	/// the number of returned errors with the total number of input cells
	///
	/// # Arguments
	///
	/// * `block` - Block number
	/// * `cells` - Matrix cells to store into DHT
	pub async fn insert_cells_into_dht(&self, block: u32, cells: Vec<Cell>) -> Result<()> {
		let records: Vec<_> = cells
			.into_iter()
			.map(DHTCell)
			.map(|cell| (cell.reference(block), cell.dht_record(block, self.ttl)))
			.collect::<Vec<_>>();
		self.insert_into_dht(records, block).await
	}

	/// Inserts rows into the DHT.
	/// There is no rollback, and errors will be logged and skipped,
	/// which means that we cannot rely on error logs as alert mechanism.
	/// Returns the success rate of the PUT operations measured by dividing
	/// the number of returned errors with the total number of input rows
	///
	/// # Arguments
	///
	/// * `block` - Block number
	/// * `rows` - Matrix rows to store into DHT
	pub async fn insert_rows_into_dht(
		&self,
		block: u32,
		rows: Vec<(RowIndex, Vec<u8>)>,
	) -> Result<()> {
		let records: Vec<_> = rows
			.into_iter()
			.map(DHTRow)
			.map(|row| (row.reference(block), row.dht_record(block, self.ttl)))
			.collect::<Vec<_>>();

		self.insert_into_dht(records, block).await
	}
}
