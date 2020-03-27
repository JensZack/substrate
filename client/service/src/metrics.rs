
use crate::NetworkStatus;
use netstat2::{TcpState, ProtocolSocketInfo, iterate_sockets_info, AddressFamilyFlags, ProtocolFlags};
use prometheus_endpoint::{register, Gauge, U64, F64, Registry, PrometheusError, Opts, GaugeVec};
use sc_client::ClientInfo;
use sc_telemetry::{telemetry, SUBSTRATE_INFO};
use std::collections::HashMap;
use std::convert::TryFrom;
use sp_runtime::traits::{NumberFor, Block, SaturatedConversion, UniqueSaturatedInto};
use sp_transaction_pool::PoolStatus;
use sp_utils::metrics::GLOBAL_METRICS;
use sysinfo::{get_current_pid, ProcessExt, System, SystemExt};

struct PrometheusMetrics {
	block_height_number: GaugeVec<U64>,
	ready_transactions_number: Gauge<U64>,
	memory_usage_bytes: Gauge<U64>,
	netstat: GaugeVec<U64>,
	load_avg: GaugeVec<F64>,
	block_import: GaugeVec<U64>,
	cpu_usage_percentage: Gauge<F64>,
	network_per_sec_bytes: GaugeVec<U64>,
	database_cache: Gauge<U64>,
	state_cache: Gauge<U64>,
	state_db: GaugeVec<U64>,
	tokio: GaugeVec<U64>,
	unbounded_channels: GaugeVec<U64>,
	internals: GaugeVec<U64>,
}

impl PrometheusMetrics {
	fn setup(registry: &Registry, name: &str, version: &str, roles: u64)
		-> Result<Self, PrometheusError>
	{
        register(Gauge::<U64>::with_opts(
            Opts::new(
                "build_info",
                "A metric with a constant '1' value labeled by name, version, and commit."
            )
                .const_label("name", name)
                .const_label("version", version)
		)?, &registry)?.set(1);
		
        register(Gauge::<U64>::new(
            "node_roles", "The roles the node is running as",
		)?, &registry)?.set(roles);
		
		Ok(Self {
			block_height_number: register(GaugeVec::new(
				Opts::new("block_height_number", "Height of the chain"),
				&["status"]
			)?, registry)?,
			ready_transactions_number: register(Gauge::new(
				"ready_transactions_number", "Number of transactions in the ready queue",
			)?, registry)?,
			memory_usage_bytes: register(Gauge::new(
				"memory_usage_bytes", "Node memory usage",
			)?, registry)?,
			cpu_usage_percentage: register(Gauge::new(
				"cpu_usage_percentage", "Node CPU usage",
			)?, registry)?,
			network_per_sec_bytes: register(GaugeVec::new(
				Opts::new("network_per_sec_bytes", "Networking bytes per second"),
				&["direction"]
			)?, registry)?,
			database_cache: register(Gauge::new(
				"database_cache_bytes", "RocksDB cache size in bytes",
			)?, registry)?,
			state_cache: register(Gauge::new(
				"state_cache_bytes", "State cache size in bytes",
			)?, registry)?,
			state_db: register(GaugeVec::new(
				Opts::new("state_db_cache_bytes", "State DB cache in bytes"),
				&["subtype"]
			)?, registry)?,
			netstat: register(GaugeVec::new(
				Opts::new("netstat_tcp", "Current TCP connections "),
				&["status"]
			)?, registry)?,
			load_avg: register(GaugeVec::new(
				Opts::new("load_avg", "System load average"),
				&["over"]
			)?, registry)?,
			block_import: register(GaugeVec::new(
				Opts::new("block_import", "Block Import"),
				&["subtype"]
			)?, registry)?,
			tokio: register(GaugeVec::new(
				Opts::new("tokio", "Tokio internals"),
				&["entity"]
			)?, registry)?,
			internals: register(GaugeVec::new(
				Opts::new("internals", "Other unspecified internals"),
				&["entity"]
			)?, registry)?,
			unbounded_channels: register(GaugeVec::new(
				Opts::new("internals_unbounded_channels", "items in each mpsc::unbounded instance"),
				&["entity"]
			)?, registry)?,
		})
	}
}

pub struct MetricsService {
	metrics: Option<PrometheusMetrics>,
	system: sysinfo::System,
	pid: Option<sysinfo::Pid>,
}

#[derive(Default)]
struct ConnectionsCount {
	listen: u64,
	established: u64,
	starting: u64,
	closing: u64,
	closed: u64,
	other: u64
}

struct TimeSeriesInfo {
	count: u64,
	lower_median: u64,
	median: u64,
	higher_median: u64,
	average: u64
}


impl From<Vec<u64>> for TimeSeriesInfo {
	fn from(mut input: Vec<u64>) -> Self {
		let count = input.len();
		if let Some(only_value) = match count {
			0 => Some(0),
			1 => Some(input[0]),
			_ => None
		} {
			return TimeSeriesInfo {
				count: u64::try_from(count).expect("Usize always fits into u64. qed"),
				lower_median: only_value,
				median: only_value,
				higher_median: only_value,
				average: only_value
			}
		}

		input.sort();
		let median_pos = count.div_euclid(2);
		let median_dif = median_pos.div_euclid(2);
		let count = u64::try_from(count).expect("Usize always fits into u64. qed");
		let average = input.iter().fold(0u64, |acc, val| acc + val).div_euclid(count);

		TimeSeriesInfo {
			count,
			lower_median: input[median_pos - median_dif],
			median: input[median_pos],
			higher_median: input[median_pos + median_dif],
			average
		}
	}
}



impl MetricsService {

	pub fn with_prometheus(registry: &Registry, name: &str, version: &str, roles: u64)
		-> Result<Self, PrometheusError>
	{
		PrometheusMetrics::setup(registry, name, version, roles).map(|p| {
			Self::inner_new(Some(p))
		})
	}

	pub fn new() -> Self {
		Self::inner_new(None)
	}

	fn inner_new(metrics: Option<PrometheusMetrics>) -> Self {
		let system = System::new();
		let pid = get_current_pid().ok();

		Self {
			metrics,
			system,
			pid,
		}
	}

	fn process_info(&mut self) -> (f32, u64) {
		match self.pid {
			Some(pid) => if self.system.refresh_process(pid) {
					let proc = self.system.get_process(pid)
						.expect("Above refresh_process succeeds, this must be Some(), qed");
					(proc.cpu_usage(), proc.memory())
				} else { (0.0, 0) },
			None => (0.0, 0)
		}
	}

	fn connections_info(&self) -> Option<ConnectionsCount> {
		self.pid.as_ref().and_then(|pid| {
			let af_flags = AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6;
			let proto_flags = ProtocolFlags::TCP;
			let netstat_pid = *pid as u32;

			iterate_sockets_info(af_flags, proto_flags).ok().map(|iter|
				iter.filter_map(|r| 
					r.ok().and_then(|s| {
						if s.associated_pids.contains(&netstat_pid) {
							match s.protocol_socket_info {
								ProtocolSocketInfo::Tcp(info) => Some(info.state),
								_ => None
							}
						} else {
							None
						}
					})
				).fold(ConnectionsCount::default(), |mut counter, socket_state| {
					match socket_state {
						TcpState::Listen => counter.listen += 1,
						TcpState::Established => counter.established += 1,
						TcpState::Closed => counter.closed += 1,
						TcpState::SynSent | TcpState::SynReceived => counter.starting += 1,
						TcpState::FinWait1 | TcpState::FinWait2 | TcpState::CloseWait
						| TcpState::Closing | TcpState::LastAck => counter.closing += 1,
						_ => counter.other += 1
					}

					counter
				})
			)
		})
	}

	pub fn tick<T: Block>(&mut self, info: &ClientInfo<T>, txpool_status: &PoolStatus, net_status: &NetworkStatus<T>) {

		let best_number = info.chain.best_number.saturated_into::<u64>();
		let best_hash = info.chain.best_hash;
		let num_peers = net_status.num_connected_peers;
		let finalized_number: u64 = info.chain.finalized_number.saturated_into::<u64>();
		let bandwidth_download = net_status.average_download_per_sec;
		let bandwidth_upload = net_status.average_upload_per_sec;
		let best_seen_block = net_status.best_seen_block
			.map(|num: NumberFor<T>| num.unique_saturated_into() as u64);
		let (cpu_usage, memory) = self.process_info();


		telemetry!(
			SUBSTRATE_INFO;
			"system.interval";
			"peers" => num_peers,
			"height" => best_number,
			"best" => ?best_hash,
			"txcount" => txpool_status.ready,
			"cpu" => cpu_usage,
			"memory" => memory,
			"finalized_height" => finalized_number,
			"finalized_hash" => ?info.chain.finalized_hash,
			"bandwidth_download" => bandwidth_download,
			"bandwidth_upload" => bandwidth_upload,
			"used_state_cache_size" => info.usage.as_ref()
				.map(|usage| usage.memory.state_cache.as_bytes())
				.unwrap_or(0),
			"used_db_cache_size" => info.usage.as_ref()
				.map(|usage| usage.memory.database_cache.as_bytes())
				.unwrap_or(0),
			"disk_read_per_sec" => info.usage.as_ref()
				.map(|usage| usage.io.bytes_read)
				.unwrap_or(0),
			"disk_write_per_sec" => info.usage.as_ref()
				.map(|usage| usage.io.bytes_written)
				.unwrap_or(0),
		);

		// consume the series, whether there is prometheus or not,to not leak memory here
		let series = GLOBAL_METRICS.flush_series();

		if let Some(metrics) = self.metrics.as_ref() {
			metrics.cpu_usage_percentage.set(cpu_usage as f64);
			metrics.memory_usage_bytes.set(memory);
			metrics.ready_transactions_number.set(txpool_status.ready as u64);

			let load = self.system.get_load_average();
			metrics.load_avg.with_label_values(&["1min"]).set(load.one);
			metrics.load_avg.with_label_values(&["5min"]).set(load.five);
			metrics.load_avg.with_label_values(&["15min"]).set(load.fifteen);

			metrics.network_per_sec_bytes.with_label_values(&["download"]).set(net_status.average_download_per_sec);
			metrics.network_per_sec_bytes.with_label_values(&["upload"]).set(net_status.average_upload_per_sec);

			metrics.block_height_number.with_label_values(&["finalized"]).set(finalized_number);
			metrics.block_height_number.with_label_values(&["best"]).set(best_number);

			if let Some(best_seen_block) = best_seen_block {
				metrics.block_height_number.with_label_values(&["sync_target"]).set(best_seen_block);
			}

			if let Some(info) = info.usage.as_ref() {
				metrics.database_cache.set(info.memory.database_cache.as_bytes() as u64);
				metrics.state_cache.set(info.memory.state_cache.as_bytes() as u64);

				metrics.state_db.with_label_values(&["non_canonical"]).set(info.memory.state_db.non_canonical.as_bytes() as u64);
				if let Some(pruning) = info.memory.state_db.pruning {
					metrics.state_db.with_label_values(&["pruning"]).set(pruning.as_bytes() as u64);
				}
				metrics.state_db.with_label_values(&["pinned"]).set(info.memory.state_db.pinned.as_bytes() as u64);
			}

			if let Some(conns) = self.connections_info() {
				metrics.netstat.with_label_values(&["listen"]).set(conns.listen);
				metrics.netstat.with_label_values(&["established"]).set(conns.established);
				metrics.netstat.with_label_values(&["starting"]).set(conns.starting);
				metrics.netstat.with_label_values(&["closing"]).set(conns.closing);
				metrics.netstat.with_label_values(&["closed"]).set(conns.closed);
				metrics.netstat.with_label_values(&["other"]).set(conns.other);
			}

			GLOBAL_METRICS.inner().read().iter().for_each(|(key, value)| {
				if key.starts_with("tokio_") {
					metrics.tokio.with_label_values(&[&key[6..]]).set(*value);
				} else if key.starts_with("mpsc_") {
					metrics.unbounded_channels.with_label_values(&[&key[5..]]).set(*value);
				} else {
					metrics.internals.with_label_values(&[&key[..]]).set(*value);
				}
			});

			let mut series = series.into_iter().fold(HashMap::<&'static str, Vec<u64>>::new(),
				| mut h, (key, value)| {
					h.entry(key)
						.and_modify(|v| {
							v.push(value)
						})
						.or_insert(vec![value]);
					h
				}
			);

			if let Some(imports) = series.remove("block_imports") {
				let info = TimeSeriesInfo::from(imports);
				metrics.block_import.with_label_values(&["count"]).set(info.count);
				metrics.block_import.with_label_values(&["time_average"]).set(info.average);
				metrics.block_import.with_label_values(&["time_median"]).set(info.median);
				metrics.block_import.with_label_values(&["time_lower_median"]).set(info.lower_median);
				metrics.block_import.with_label_values(&["time_higher_median"]).set(info.higher_median);
			}

			series.into_iter().for_each(|(key, values)| {
				let info = TimeSeriesInfo::from(values);
				metrics.internals.with_label_values(&[&format!("{:}_count", key)[..]]).set(info.count);
				metrics.internals.with_label_values(&[&format!("{:}_average", key)[..]]).set(info.average);
				metrics.internals.with_label_values(&[&format!("{:}_median", key)[..]]).set(info.median);
				metrics.internals.with_label_values(&[&format!("{:}_lower_media", key)[..]]).set(info.lower_median);
				metrics.internals.with_label_values(&[&format!("{:}_higher_median", key)[..]]).set(info.higher_median);
			});
		}

	}
}