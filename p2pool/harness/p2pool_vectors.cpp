/*
 * p2pool_vectors: expected values of the C++ p2pool for real sidechain shares, one JSON line per
 * share, for the differential tests of xkrd's Rust p2pool (crates/xkr-p2pool).
 *
 *   p2pool_vectors <blocks.jsonl> <seed-diffs.json> [sidechain-config.json] > cpp-vectors.jsonl
 *
 * blocks.jsonl     one {"serialized": "<hex>"} per line (a block cache slot: main-chain part then
 *                  sidechain part), e.g. crates/xkr-p2pool/tests/data/shares-mainnet.jsonl
 * seed-diffs.json  {"seeds": {"<main-chain height>": {"difficulty": <u64>, ...}, ...}, ...}: the
 *                  main-chain difficulty at every seed height (h - 65) & ~2047 the shares use
 * sidechain-config optional p2pool/sidechain-testnet.json: its consensus id is printed too
 *
 * Model: tests/src/pool_block_tests.cpp (a SideChain without a p2pool object, blocks added with
 * add_block, m_testMainChainDiff as the main-chain difficulty of get_shares). Built in a copy of
 * the checkout at 8edf632 + patches 0001 and 0002, with `friend struct VectorDump;` added to
 * class SideChain (see Dockerfile), at -O2.
 *
 * Output lines:
 *   {"consensus_id": {"default": .., "mini": .., "nano": .., "config": ..}}
 *   {"parse_error": <line number>, "code": <deserialize() result>}
 *   {"missing_seed_difficulty": <seed height>}
 *   one object per added share (fields below), in sidechain height order
 */

#include "common.h"
#include "util.h"
#include "pool_block.h"
#include "side_chain.h"
#include "wallet.h"
#include "keccak.h"
#include "crypto.h"
#include "json_rpc_request.h"
#include "merge_mining_client.h"
#include "xkr_pow.h"

#include <rapidjson/document.h>

#include <algorithm>
#include <cstdio>
#include <fstream>
#include <iostream>
#include <map>
#include <sstream>
#include <string>
#include <vector>

// The same stubs as tests/src/main.cpp: the objects below are linked but never used here.
void p2pool_usage() {}

namespace p2pool {

IMergeMiningClient* IMergeMiningClient::create(p2pool*, const std::string&, const std::string&, const std::string&) noexcept { return nullptr; }

namespace JSONRPCRequest {
	void Call(const std::string&, int, const std::string&, const std::string&, const std::string&, bool, const std::string&, CallbackBase*, CallbackBase*, uv_loop_t*) {}
}

static std::string hex(const uint8_t* p, size_t n)
{
	static const char digits[] = "0123456789abcdef";
	std::string s;
	s.reserve(n * 2);
	for (size_t i = 0; i < n; ++i) {
		s.push_back(digits[p[i] >> 4]);
		s.push_back(digits[p[i] & 15]);
	}
	return s;
}

static std::string hex(const hash& h) { return hex(h.h, HASH_SIZE); }

static std::string dec(const difficulty_type& d)
{
	unsigned __int128 v = (static_cast<unsigned __int128>(d.hi) << 64) | d.lo;
	if (v == 0) {
		return "0";
	}
	std::string s;
	while (v) {
		s.push_back(static_cast<char>('0' + static_cast<int>(v % 10)));
		v /= 10;
	}
	std::reverse(s.begin(), s.end());
	return s;
}

static bool unhex(const std::string& s, std::vector<uint8_t>& out)
{
	if (s.size() % 2) {
		return false;
	}
	out.resize(s.size() / 2);
	for (size_t i = 0; i < out.size(); ++i) {
		uint8_t d[2];
		if (!from_hex(s[i * 2], d[0]) || !from_hex(s[i * 2 + 1], d[1])) {
			return false;
		}
		out[i] = static_cast<uint8_t>((d[0] << 4) | d[1]);
	}
	return true;
}

static uint64_t seed_height(uint64_t h)
{
	return (h > 64) ? ((h - 65) & ~static_cast<uint64_t>(2047)) : 0;
}

// The text SideChain::SideChain hashes for a JSON config (load_config + the consensus text).
static std::string config_consensus_id(const char* path)
{
	std::ifstream f(path);
	std::stringstream ss;
	ss << f.rdbuf();
	rapidjson::Document doc;
	doc.Parse<rapidjson::kParseCommentsFlag>(ss.str().c_str());
	if (doc.HasParseError() || !doc.IsObject()) {
		return "";
	}
	std::string name = "default", password;
	uint64_t block_time = 10, min_diff = 100000, window = 2160, penalty = 20;
	if (doc.HasMember("name") && doc["name"].IsString()) name = doc["name"].GetString();
	if (doc.HasMember("password") && doc["password"].IsString()) password = doc["password"].GetString();
	if (doc.HasMember("block_time") && doc["block_time"].IsUint64()) block_time = doc["block_time"].GetUint64();
	if (doc.HasMember("min_diff") && doc["min_diff"].IsUint64() && doc["min_diff"].GetUint64()) min_diff = doc["min_diff"].GetUint64();
	if (doc.HasMember("pplns_window") && doc["pplns_window"].IsUint64()) window = doc["pplns_window"].GetUint64();
	if (doc.HasMember("uncle_penalty") && doc["uncle_penalty"].IsUint64()) penalty = doc["uncle_penalty"].GetUint64();

	std::string text = "mainnet";
	text.push_back('\0');
	for (const std::string& part : { name, password, std::to_string(block_time), std::to_string(min_diff), std::to_string(window), std::to_string(penalty) }) {
		text += part;
		text.push_back('\0');
	}
	hash id;
	xkr_cn_turtle_pow(text.data(), text.size(), id.h);
	return hex(id);
}

struct VectorDump
{
	static bool shares(const SideChain& s, const PoolBlock* b, std::vector<MinerShare>& out, uint64_t& bottom)
	{
		return s.get_shares(b, out, &bottom, true);
	}

	static bool difficulty(const SideChain& s, const PoolBlock* b, difficulty_type& out)
	{
		std::vector<DifficultyData> data;
		return s.get_difficulty(b, data, out);
	}
};

static int run(int argc, char** argv)
{
	if (argc < 3) {
		std::cerr << "usage: p2pool_vectors <blocks.jsonl> <seed-diffs.json> [sidechain-config.json]\n";
		return 2;
	}

	PoolBlock::s_coinbaseUnlockWindow = 20;

	// Seed difficulties.
	std::map<uint64_t, difficulty_type> seed_diffs;
	{
		std::ifstream f(argv[2]);
		std::stringstream ss;
		ss << f.rdbuf();
		rapidjson::Document doc;
		doc.Parse(ss.str().c_str());
		if (doc.HasParseError() || !doc.IsObject() || !doc.HasMember("seeds") || !doc["seeds"].IsObject()) {
			std::cerr << "seed-diffs.json: expected {\"seeds\": {\"<height>\": {\"difficulty\": n}}}\n";
			return 2;
		}
		for (auto it = doc["seeds"].MemberBegin(); it != doc["seeds"].MemberEnd(); ++it) {
			const uint64_t h = std::stoull(it->name.GetString());
			const auto& v = it->value;
			uint64_t d = 0;
			if (v.IsObject() && v.HasMember("difficulty") && v["difficulty"].IsUint64()) {
				d = v["difficulty"].GetUint64();
			}
			else if (v.IsUint64()) {
				d = v.GetUint64();
			}
			seed_diffs[h] = difficulty_type(d, 0);
		}
	}

	{
		SideChain mini(nullptr, NetworkType::Mainnet, "mini");
		SideChain nano(nullptr, NetworkType::Mainnet, "nano");
		SideChain def(nullptr, NetworkType::Mainnet, "default");
		std::cout << "{\"consensus_id\":{\"default\":\"" << hex(def.consensus_id().data(), HASH_SIZE)
			<< "\",\"mini\":\"" << hex(mini.consensus_id().data(), HASH_SIZE)
			<< "\",\"nano\":\"" << hex(nano.consensus_id().data(), HASH_SIZE) << '"';
		if (argc > 3) {
			std::cout << ",\"config\":\"" << config_consensus_id(argv[3]) << '"';
		}
		std::cout << "}}\n";
	}

	SideChain sidechain(nullptr, NetworkType::Mainnet, "default");

	std::vector<PoolBlock*> blocks;
	{
		std::ifstream f(argv[1]);
		std::string line;
		uint64_t line_no = 0;
		while (std::getline(f, line)) {
			++line_no;
			const size_t key = line.find("\"serialized\"");
			if (key == std::string::npos) {
				continue;
			}
			const size_t a = line.find('"', line.find(':', key) + 1);
			const size_t b = line.find('"', a + 1);
			std::vector<uint8_t> data;
			if (a == std::string::npos || b == std::string::npos || !unhex(line.substr(a + 1, b - a - 1), data)) {
				std::cout << "{\"parse_error\":" << line_no << ",\"code\":-1}\n";
				continue;
			}
			PoolBlock* block = new PoolBlock();
			const int r = block->deserialize(data.data(), data.size(), sidechain, false, false);
			if (r) {
				std::cout << "{\"parse_error\":" << line_no << ",\"code\":" << r << "}\n";
				delete block;
				continue;
			}
			blocks.push_back(block);
		}
	}

	std::stable_sort(blocks.begin(), blocks.end(), [](const PoolBlock* x, const PoolBlock* y) { return x->m_sidechainHeight < y->m_sidechainHeight; });

	auto set_main_diff = [&](const PoolBlock* b) -> bool {
		const uint64_t h = seed_height(b->m_txinGenHeight);
		auto it = seed_diffs.find(h);
		if (it == seed_diffs.end()) {
			std::cout << "{\"missing_seed_difficulty\":" << h << "}\n";
			sidechain.m_testMainChainDiff = {};
			return false;
		}
		sidechain.m_testMainChainDiff = it->second;
		return true;
	};

	std::vector<hash> ids;
	for (PoolBlock* b : blocks) {
		set_main_diff(b);
		ids.push_back(b->m_sidechainId);
		if (!sidechain.add_block(*b)) {
			std::cout << "{\"duplicate\":\"" << hex(b->m_sidechainId) << "\"}\n";
		}
		delete b;
	}
	blocks.clear();

	for (const hash& id : ids) {
		const PoolBlock* s = sidechain.find_block(id);
		if (!s) {
			continue;
		}
		const bool have_seed = set_main_diff(s);

		PoolBlock copy(*s);
		uint8_t blob[HASHING_BLOB_MAX_SIZE];
		size_t nonce_offset = 0;
		const size_t blob_size = copy.build_pow_blob(copy.m_extraNonce, copy.m_nonce, blob, nonce_offset);
		hash pow;
		xkr_cn_turtle_pow(blob, blob_size, pow.h);

		std::vector<uint8_t> serialized = s->serialize_mainchain_data();
		const std::vector<uint8_t> side = s->serialize_sidechain_data();
		serialized.insert(serialized.end(), side.begin(), side.end());

		std::ostringstream o;
		o << "{\"id\":\"" << hex(s->m_sidechainId) << '"'
			<< ",\"height\":" << s->m_sidechainHeight
			<< ",\"txin_gen_height\":" << s->m_txinGenHeight
			<< ",\"seed_height\":" << seed_height(s->m_txinGenHeight)
			<< ",\"prev_id\":\"" << hex(s->m_prevId) << '"'
			<< ",\"parent\":\"" << hex(s->m_parent) << '"'
			<< ",\"uncles\":[";
		for (size_t i = 0; i < s->m_uncles.size(); ++i) {
			o << (i ? "," : "") << '"' << hex(s->m_uncles[i]) << '"';
		}
		o << "],\"difficulty\":\"" << dec(s->m_difficulty) << '"'
			<< ",\"cumulative_difficulty\":\"" << dec(s->m_cumulativeDifficulty) << '"'
			<< ",\"timestamp\":" << s->m_timestamp
			<< ",\"nonce\":" << s->m_nonce
			<< ",\"extra_nonce\":" << s->m_extraNonce
			<< ",\"txkey_sec_seed\":\"" << hex(s->m_txkeySecSeed) << '"'
			<< ",\"txkey_pub\":\"" << hex(s->m_txkeyPub) << '"'
			<< ",\"txkey_sec\":\"" << hex(s->m_txkeySec) << '"'
			<< ",\"tx_key_seed\":\"" << hex(s->calculate_tx_key_seed()) << '"'
			<< ",\"pow_hash\":\"" << hex(pow) << '"'
			<< ",\"pow_blob\":\"" << hex(blob, blob_size) << '"'
			<< ",\"verified\":" << (s->m_verified ? 1 : 0)
			<< ",\"invalid\":" << (s->m_invalid ? 1 : 0)
			<< ",\"depth\":" << s->m_depth;

		const difficulty_type cached = sidechain.get_cached_next_difficulty(id);
		if (cached.empty()) {
			o << ",\"cached_next_difficulty\":null";
		}
		else {
			o << ",\"cached_next_difficulty\":\"" << dec(cached) << '"';
		}
		difficulty_type next;
		if (VectorDump::difficulty(sidechain, s, next)) {
			o << ",\"next_difficulty\":\"" << dec(next) << '"';
		}
		else {
			o << ",\"next_difficulty\":null";
		}

		std::vector<MinerShare> shares;
		uint64_t bottom = 0;
		if (have_seed && VectorDump::shares(sidechain, s, shares, bottom)) {
			// Does the window reach into another seed epoch (another main-chain difficulty)?
			bool mixed = false;
			const uint64_t epoch = seed_height(s->m_txinGenHeight);
			for (const PoolBlock* cur = s; cur && (cur->m_sidechainHeight >= bottom); cur = sidechain.find_block(cur->m_parent)) {
				if (seed_height(cur->m_txinGenHeight) != epoch) {
					mixed = true;
				}
				if (cur->m_sidechainHeight == 0) {
					break;
				}
			}
			o << ",\"main_chain_difficulty\":\"" << dec(sidechain.m_testMainChainDiff) << '"'
				<< ",\"window_bottom\":" << bottom
				<< ",\"epoch_mixed\":" << (mixed ? 1 : 0)
				<< ",\"shares\":[";
			for (size_t i = 0; i < shares.size(); ++i) {
				o << (i ? "," : "") << "{\"spend\":\"" << hex(shares[i].m_wallet->spend_public_key())
					<< "\",\"view\":\"" << hex(shares[i].m_wallet->view_public_key())
					<< "\",\"weight\":\"" << dec(shares[i].m_weight) << "\"}";
			}
			o << ']';

			uint64_t total = 0;
			for (const auto& out : s->m_outputAmounts) {
				total += out.m_reward;
			}
			std::vector<uint64_t> rewards;
			if (SideChain::split_reward(total, shares, rewards)) {
				o << ",\"split_reward\":[";
				for (size_t i = 0; i < rewards.size(); ++i) {
					o << (i ? "," : "") << rewards[i];
				}
				o << ']';
			}
			else {
				o << ",\"split_reward\":null";
			}
		}
		else {
			o << ",\"shares\":null";
		}

		o << ",\"outputs\":[";
		for (size_t i = 0; i < s->m_outputAmounts.size(); ++i) {
			o << (i ? "," : "") << "{\"amount\":" << s->m_outputAmounts[i].m_reward
				<< ",\"key\":\"" << hex(s->m_ephPublicKeys[i]) << "\"}";
		}
		o << "],\"serialized\":\"" << hex(serialized.data(), serialized.size()) << "\"}";
		std::cout << o.str() << '\n';
	}

	return 0;
}

} // namespace p2pool

int main(int argc, char** argv)
{
	memset((void*) &robin_hood::SIPHASH_KEY, 0, sizeof(robin_hood::SIPHASH_KEY));
	p2pool::set_main_thread();
	p2pool::init_crypto_cache();
	p2pool::PoolBlock::s_precalculatedSharesLock = new p2pool::ReadWriteLock();

	const int result = p2pool::run(argc, argv);

	// Exit without tearing the sidechain down: the process ends here.
	std::cout.flush();
	std::_Exit(result);
}
