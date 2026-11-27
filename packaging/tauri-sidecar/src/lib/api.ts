// Typed wrappers around the Rust commands (src-tauri/src/main.rs).
import { invoke } from '@tauri-apps/api/core';

export type Phase = 'starting' | 'running' | 'stopping' | 'stopped' | 'exited' | 'failed';
export type MineMode = 'solo' | 'p2pool';

export interface Settings {
	testnet: boolean;
	fast_sync: boolean;
	verify_threads: number;
	hyper: boolean;
	hugin: boolean;
	payout_address: string;
	mining: { enabled: boolean; mode: MineMode; address: string; threads: number };
	p2pool: { enabled: boolean; sidechain: 'default' | 'mini' | 'nano'; stratum_lan: boolean; stratum_port: number };
	wallet_node: string;
	extra_args: string[];
}

export interface NodeInfo {
	phase: Phase;
	detail: string;
	rpc_port: number;
	cores: number;
	default_threads: number;
	settings: Settings;
	data_dir: string;
	app_data: string;
	log_file: string;
	hyper_available: boolean;
}

export interface MiningStatus {
	active: boolean;
	mode: string;
	address: string;
	threads: number;
	hashrate: number;
	total_hashes: number;
	height: number;
	difficulty: number;
	blocks_found: number;
	shares_found: number;
	blocks_rejected: number;
	last_block_hash: string | null;
	last_error: string | null;
}

export interface P2poolStatus {
	state: 'waiting' | 'running' | 'stopped' | 'failed';
	sidechain: string;
	consensus_id: string;
	height: number;
	difficulty: number;
	hashrate: number;
	miners: number;
	shares: number;
	synced: boolean;
	peers_out: number;
	peers_in: number;
	peers_hyper: number;
	stratum_miners: number;
	stratum_hashrate: number;
	shares_found: number;
	blocks_found: number;
	payout_address: string | null;
	payout_donation: boolean;
	stratum: string | null;
	stratum_off: string | null;
	error: string | null;
}

// xkrd's /status; nested objects the app only shows as they are stay loosely typed.
export interface NodeStatus {
	version: string;
	network: string;
	height: number;
	top_block_hash: string;
	top_block_timestamp: number;
	network_height: number;
	synced: boolean;
	sync_progress: number;
	peers: { legacy_outgoing: number; legacy_incoming: number; hyper: number; total: number };
	hyper: Record<string, unknown> | null;
	mining: MiningStatus | null;
	hugin: Record<string, unknown> | null;
	p2pool: P2poolStatus | null;
	white_peerlist_size: number;
	gray_peerlist_size: number;
	download: { active: boolean; blocks_per_second: number; bytes_per_second: number };
	pool: { count: number; bytes: number; fusion: number };
	alt_blocks: number;
	fork: Record<string, unknown>;
	block_rate: { window_secs: number; blocks: number; expected: number; ratio: number; status: string };
	db_size: number;
	data_dir: string | null;
	start_time: number;
	uptime: number;
}

export interface WalletOverview {
	open: boolean;
	name?: string;
	path?: string;
	format?: string;
	address?: string;
	addresses?: string[];
	view_only?: boolean;
	balance?: { unlocked: number; locked: number };
	sync?: {
		wallet_height: number;
		node_height: number;
		network_height: number;
		daemon_online: boolean;
		peers: number;
		hashrate: number;
	};
	node?: { host: string; port: number; fee_address: string; fee_amount: number };
}

export interface WalletTx {
	hash: string;
	amount: number;
	fee: number;
	height: number;
	timestamp: number;
	payment_id: string;
	coinbase: boolean;
	confirmed: boolean;
}

export interface WalletFile {
	name: string;
	path: string;
	modified: number;
}

export const api = {
	nodeInfo: () => invoke<NodeInfo>('node_info'),
	nodeStatus: () => invoke<NodeStatus>('node_status'),
	nodeLog: (lines: number) => invoke<string>('node_log', { lines }),
	nodeRestart: () => invoke<void>('node_restart'),
	updateSettings: (settings: Settings) => invoke<void>('update_settings', { settings }),
	walletList: () => invoke<{ dir: string; wallets: WalletFile[] }>('wallet_list'),
	walletCreate: (name: string, password: string) =>
		invoke<{ address: string; mnemonic: string }>('wallet_create', { name, password }),
	walletImportSeed: (name: string, password: string, mnemonic: string, scanHeight: number) =>
		invoke<void>('wallet_import_seed', { name, password, mnemonic, scanHeight }),
	walletOpen: (path: string, password: string) => invoke<void>('wallet_open', { path, password }),
	walletClose: () => invoke<void>('wallet_close'),
	walletOverview: () => invoke<WalletOverview>('wallet_overview'),
	walletTransactions: (limit: number) =>
		invoke<{ total: number; transactions: WalletTx[] }>('wallet_transactions', { limit }),
	walletSend: (address: string, amount: number, paymentId: string) =>
		invoke<string>('wallet_send', { address, amount, paymentId }),
	walletMnemonic: (password: string) => invoke<string>('wallet_mnemonic', { password }),
	walletRescan: (height: number) => invoke<void>('wallet_rescan', { height }),
	checkAddress: (address: string) => invoke<void>('check_address', { address })
};
