// Differential harness driver (same source for every backend, see backend.h and Makefile).
//
//   driver gen <seed> <count>   print a deterministic command list (no crypto, PRNG only)
//   driver run                  read commands from stdin, print "<command> => <result>" lines;
//                               signing commands also print "VERIFY <command>" lines that
//                               another backend must accept (signatures are random, so they
//                               are compared by cross-verification, not by value)
//   driver vectors <file>...    check known-answer files (monero tests.txt format and
//                               compat/kat.txt); exit 1 on any failure
//
// Hex arguments are lower case; "-" stands for empty data.
#include "backend.h"

#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <iostream>
#include <sstream>
#include <string>
#include <vector>

using backend::Hex;
using backend::Opt;

namespace
{
    /* splitmix64, as in tests/compat_dump.rs */
    struct Rng
    {
        uint64_t state;

        uint64_t next()
        {
            state += 0x9e3779b97f4a7c15ULL;
            uint64_t z = state;
            z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
            z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
            return z ^ (z >> 31);
        }

        uint64_t below(uint64_t n)
        {
            return next() % n;
        }

        Hex bytes(size_t len)
        {
            static const char *digits = "0123456789abcdef";
            Hex out;
            out.reserve(2 * len);
            uint64_t word = 0;
            for (size_t i = 0; i < len; i++)
            {
                if (i % 8 == 0)
                {
                    word = next();
                }
                const auto byte = static_cast<uint8_t>(word >> (8 * (i % 8)));
                out.push_back(digits[byte >> 4]);
                out.push_back(digits[byte & 15]);
            }
            return out;
        }
    };

    std::string data(const Hex &h)
    {
        return h.empty() ? "-" : h;
    }

    Hex undash(const std::string &token)
    {
        return token == "-" ? "" : token;
    }

    std::string show(const Opt &o)
    {
        return o ? *o : "ERR";
    }

    std::string show(bool b)
    {
        return b ? "true" : "false";
    }

    std::vector<std::string> tokens(const std::string &line)
    {
        std::istringstream in(line);
        std::vector<std::string> out;
        std::string t;
        while (in >> t)
        {
            out.push_back(t);
        }
        return out;
    }

    uint64_t num(const std::string &s)
    {
        return std::strtoull(s.c_str(), nullptr, 10);
    }

    /* A canonical scalar from 32 random bytes (the backend's own sc_reduce32). */
    Hex scalar(const Hex &seed)
    {
        return show(backend::scReduce32(seed));
    }

    char flipHex(char c)
    {
        return c == '0' ? '1' : '0';
    }

    // ------------------------------------------------------------------------------------
    // gen

    void gen(uint64_t seed, uint64_t count)
    {
        Rng rng {seed};
        for (uint64_t i = 0; i < count; i++)
        {
            std::cout << "ck " << rng.bytes(32) << "\n";
            std::cout << "sk2pk " << rng.bytes(32) << "\n";
            std::cout << "scr " << rng.bytes(32) << "\n";
            std::cout << "h2ec " << rng.bytes(32) << "\n";
            std::cout << "h2s " << data(rng.bytes(rng.below(100))) << "\n";
            std::cout << "fast " << data(rng.bytes(rng.below(300))) << "\n";
            std::cout << "kd " << rng.bytes(32) << " " << rng.bytes(32) << "\n";
            std::cout << "dpk " << rng.bytes(32) << " " << rng.below(5000) << " " << rng.bytes(32) << "\n";
            std::cout << "dsk " << rng.bytes(32) << " " << rng.below(5000) << " " << rng.bytes(32) << "\n";
            std::cout << "upk " << rng.bytes(32) << " " << rng.below(5000) << " " << rng.bytes(32) << "\n";
            std::cout << "ki " << rng.bytes(32) << " " << rng.bytes(32) << "\n";
            std::cout << "sub " << rng.bytes(32) << " " << rng.below(40) << "\n";
            if (i % 4 == 0)
            {
                std::cout << "chain " << rng.bytes(32) << " " << rng.bytes(32) << " " << rng.below(300) << "\n";
                std::cout << "sign " << rng.bytes(32) << " " << rng.bytes(32) << "\n";
            }
            if (i % 8 == 0)
            {
                const uint64_t n = rng.below(6) + 1;
                std::cout << "ring " << rng.bytes(32) << " " << n << " " << rng.below(n);
                for (uint64_t k = 0; k < n; k++)
                {
                    std::cout << " " << rng.bytes(32);
                }
                std::cout << "\n";
            }
            if (i % 10 == 0)
            {
                std::cout << "slow " << rng.bytes(43 + rng.below(80)) << "\n";
            }
            if (i % 16 == 0)
            {
                std::cout << "block " << rng.next() << " " << (rng.below(6) + 1) << "\n";
            }
        }
    }

    // ------------------------------------------------------------------------------------
    // run

    struct Wallet
    {
        Hex viewSecret, viewPublic;
        std::vector<Hex> spendSecrets, spendPublics;
    };

    Wallet makeWallet(Rng &rng, size_t subwallets)
    {
        Wallet w;
        w.viewSecret = scalar(rng.bytes(32));
        w.viewPublic = show(backend::secretKeyToPublicKey(w.viewSecret));
        for (size_t i = 0; i < subwallets; i++)
        {
            w.spendSecrets.push_back(scalar(rng.bytes(32)));
            w.spendPublics.push_back(show(backend::secretKeyToPublicKey(w.spendSecrets.back())));
        }
        return w;
    }

    backend::Tx makeTx(Rng &rng, const std::vector<const Wallet *> &owners)
    {
        backend::Tx tx;
        const Hex r = scalar(rng.bytes(32));
        tx.txPublicKey = show(backend::secretKeyToPublicKey(r));
        tx.hash = rng.bytes(32);
        const uint64_t n = rng.below(8) + 1;
        for (uint64_t i = 0; i < n; i++)
        {
            const Wallet *w = owners[rng.below(owners.size())];
            const size_t sub = rng.below(w->spendPublics.size());
            const Hex d = show(backend::generateKeyDerivation(w->viewPublic, r));
            backend::Output o;
            o.key = show(backend::derivePublicKey(d, i, w->spendPublics[sub]));
            o.amount = rng.below(1ULL << 40);
            o.globalIndex = rng.below(8) == 0 ? -1 : static_cast<int64_t>(rng.below(1ULL << 31));
            tx.outputs.push_back(o);
        }
        return tx;
    }

    std::string block(uint64_t seed, uint64_t txCount)
    {
        Rng rng {seed};
        const Wallet me = makeWallet(rng, 3);
        const Wallet other = makeWallet(rng, 1);
        const std::vector<const Wallet *> owners {&me, &me, &other};
        const backend::Tx coinbase = makeTx(rng, owners);
        std::vector<backend::Tx> txs;
        for (uint64_t i = 0; i < txCount; i++)
        {
            txs.push_back(makeTx(rng, owners));
        }
        std::vector<backend::SpendKey> keys;
        for (size_t i = 0; i < me.spendPublics.size(); i++)
        {
            keys.push_back({me.spendPublics[i], me.spendSecrets[i]});
        }
        std::string out;
        out += show(backend::processBlockOutputs(coinbase, txs, me.viewSecret, keys, false, true));
        out += " | ";
        out += show(backend::processBlockOutputs(coinbase, txs, me.viewSecret, keys, false, false));
        out += " | ";
        out += show(backend::processBlockOutputs(coinbase, txs, me.viewSecret, keys, true, true));
        return out;
    }

    /* Returns the result text; may print VERIFY lines first. */
    std::string runCommand(const std::vector<std::string> &t)
    {
        const std::string &c = t[0];
        if (c == "ck")
        {
            return show(backend::checkKey(t[1]));
        }
        if (c == "sk2pk")
        {
            return show(backend::secretKeyToPublicKey(t[1]));
        }
        if (c == "scr")
        {
            return show(backend::scReduce32(t[1]));
        }
        if (c == "h2ec")
        {
            return show(backend::hashToEllipticCurve(t[1]));
        }
        if (c == "h2s")
        {
            return show(backend::hashToScalar(undash(t[1])));
        }
        if (c == "fast")
        {
            return show(backend::cnFastHash(undash(t[1])));
        }
        if (c == "slow")
        {
            return show(backend::cnTurtleLiteSlowHashV2(undash(t[1])));
        }
        if (c == "kd")
        {
            return show(backend::generateKeyDerivation(t[1], scalar(t[2])));
        }
        if (c == "dpk")
        {
            return show(backend::derivePublicKey(t[1], num(t[2]), t[3]));
        }
        if (c == "dsk")
        {
            return show(backend::deriveSecretKey(t[1], num(t[2]), scalar(t[3])));
        }
        if (c == "upk")
        {
            return show(backend::underivePublicKey(t[1], num(t[2]), t[3]));
        }
        if (c == "ki")
        {
            return show(backend::generateKeyImage(t[1], scalar(t[2])));
        }
        if (c == "sub")
        {
            const auto keys = backend::generateDeterministicSubwalletKeys(scalar(t[1]), num(t[2]));
            return keys ? keys->first + " " + keys->second : "ERR";
        }
        if (c == "chain")
        {
            const Hex a = scalar(t[1]);
            const Hex b = scalar(t[2]);
            const uint64_t i = num(t[3]);
            const Hex A = show(backend::secretKeyToPublicKey(a));
            const Hex B = show(backend::secretKeyToPublicKey(b));
            const Hex D = show(backend::generateKeyDerivation(A, b));
            const Hex P = show(backend::derivePublicKey(D, i, B));
            const Hex x = show(backend::deriveSecretKey(D, i, b));
            const Hex U = show(backend::underivePublicKey(D, i, P));
            const Hex KI = show(backend::generateKeyImage(P, x));
            const Hex X = show(backend::secretKeyToPublicKey(x));
            const bool consistent = U == B && X == P;
            return A + " " + B + " " + D + " " + P + " " + x + " " + KI + (consistent ? " consistent" : " INCONSISTENT");
        }
        if (c == "sign")
        {
            const Hex x = scalar(t[1]);
            const Hex &msg = t[2];
            const Hex P = show(backend::secretKeyToPublicKey(x));
            const Opt sig = backend::generateSignature(msg, P, x);
            if (!sig)
            {
                return "ERR";
            }
            Hex bad = *sig;
            bad[127] = flipHex(bad[127]);
            std::cout << "VERIFY chksig " << msg << " " << P << " " << *sig << " true\n";
            std::cout << "VERIFY chksig " << msg << " " << P << " " << bad << " "
                      << show(backend::checkSignature(msg, P, bad)) << "\n";
            return "own=" + show(backend::checkSignature(msg, P, *sig));
        }
        if (c == "chksig")
        {
            return show(backend::checkSignature(t[1], t[2], t[3])) == t[4] ? "ok" : "MISMATCH";
        }
        if (c == "ring")
        {
            const Hex &msg = t[1];
            const uint64_t n = num(t[2]);
            const uint64_t real = num(t[3]);
            std::vector<Hex> secrets, keys;
            for (uint64_t k = 0; k < n; k++)
            {
                secrets.push_back(scalar(t[4 + k]));
                keys.push_back(show(backend::secretKeyToPublicKey(secrets.back())));
            }
            const Hex image = show(backend::generateKeyImage(keys[real], secrets[real]));
            const auto sigs = backend::generateRingSignatures(msg, image, keys, secrets[real], real);
            if (!sigs)
            {
                return "ERR";
            }
            auto bad = *sigs;
            bad[0][127] = flipHex(bad[0][127]);
            const std::vector<Hex> *sets[] = {&*sigs, &bad};
            for (const auto *set : sets)
            {
                std::cout << "VERIFY chkring " << msg << " " << image << " " << n;
                for (const auto &k : keys)
                {
                    std::cout << " " << k;
                }
                for (const auto &s : *set)
                {
                    std::cout << " " << s;
                }
                std::cout << " " << show(backend::checkRingSignature(msg, image, keys, *set)) << "\n";
            }
            return "image=" + image + " own=" + show(backend::checkRingSignature(msg, image, keys, *sigs));
        }
        if (c == "chkring")
        {
            const uint64_t n = num(t[3]);
            const std::vector<Hex> keys(t.begin() + 4, t.begin() + 4 + n);
            const std::vector<Hex> sigs(t.begin() + 4 + n, t.begin() + 4 + 2 * n);
            return show(backend::checkRingSignature(t[1], t[2], keys, sigs)) == t[4 + 2 * n] ? "ok" : "MISMATCH";
        }
        if (c == "block")
        {
            return block(num(t[1]), num(t[2]));
        }
        return "UNKNOWN";
    }

    int run()
    {
        std::string line;
        while (std::getline(std::cin, line))
        {
            const auto t = tokens(line);
            if (t.empty())
            {
                continue;
            }
            const std::string result = runCommand(t);
            std::cout << line << " => " << result << "\n";
        }
        return 0;
    }

    // ------------------------------------------------------------------------------------
    // vectors

    bool flagOf(const std::string &s)
    {
        return s == "true";
    }

    /* true if the line passes */
    bool checkVector(const std::vector<std::string> &f)
    {
        const std::string &c = f[0];
        if (c == "secret_key_to_public_key")
        {
            const Opt got = backend::secretKeyToPublicKey(f[1]);
            return flagOf(f[2]) ? got && *got == f[3] : !got;
        }
        if (c == "generate_key_derivation")
        {
            const Opt got = backend::generateKeyDerivation(f[1], f[2]);
            return flagOf(f[3]) ? got && *got == f[4] : !got;
        }
        if (c == "derive_public_key")
        {
            const Opt got = backend::derivePublicKey(f[1], num(f[2]), f[3]);
            return flagOf(f[4]) ? got && *got == f[5] : !got;
        }
        if (c == "derive_secret_key")
        {
            return show(backend::deriveSecretKey(f[1], num(f[2]), f[3])) == f[4];
        }
        if (c == "generate_key_image")
        {
            return show(backend::generateKeyImage(f[1], f[2])) == f[3];
        }
        if (c == "check_ring_signature")
        {
            const uint64_t n = num(f[3]);
            const std::vector<Hex> keys(f.begin() + 4, f.begin() + 4 + n);
            std::vector<Hex> sigs;
            for (uint64_t i = 0; i < n; i++)
            {
                sigs.push_back(f[4 + n].substr(128 * i, 128));
            }
            return backend::checkRingSignature(f[1], f[2], keys, sigs) == flagOf(f[5 + n]);
        }
        if (c == "cn_fast_hash")
        {
            return show(backend::cnFastHash(undash(f[1]))) == f[2];
        }
        if (c == "cn_turtle_lite_slow_hash_v2")
        {
            return show(backend::cnTurtleLiteSlowHashV2(undash(f[1]))) == f[2];
        }
        if (c == "check_key")
        {
            return backend::checkKey(f[1]) == flagOf(f[2]);
        }
        if (c == "underive_public_key")
        {
            return show(backend::underivePublicKey(f[1], num(f[2]), f[3])) == f[4];
        }
        if (c == "subwallet_private_key")
        {
            const auto keys = backend::generateDeterministicSubwalletKeys(f[1], num(f[2]));
            return keys && keys->first == f[3];
        }
        if (c == "sc_reduce32")
        {
            return show(backend::scReduce32(f[1])) == f[2];
        }
        std::cerr << "unknown vector command " << c << "\n";
        return false;
    }

    int vectors(int argc, char **argv)
    {
        size_t pass = 0, fail = 0;
        for (int a = 2; a < argc; a++)
        {
            std::ifstream in(argv[a]);
            if (!in)
            {
                std::cerr << "cannot open " << argv[a] << "\n";
                return 2;
            }
            std::string line;
            while (std::getline(in, line))
            {
                const auto f = tokens(line);
                if (f.empty() || f[0][0] == '#')
                {
                    continue;
                }
                if (checkVector(f))
                {
                    pass++;
                }
                else
                {
                    fail++;
                    std::cout << "FAIL " << line << "\n";
                }
            }
        }
        std::cout << backend::name() << ": " << pass << " vectors passed, " << fail << " failed\n";
        return fail == 0 ? 0 : 1;
    }
} // namespace

int main(int argc, char **argv)
{
    std::ios::sync_with_stdio(false);
    const std::string mode = argc > 1 ? argv[1] : "";
    if (mode == "gen" && argc == 4)
    {
        gen(num(argv[2]), num(argv[3]));
        return 0;
    }
    if (mode == "run")
    {
        return run();
    }
    if (mode == "vectors" && argc > 2)
    {
        return vectors(argc, argv);
    }
    if (mode == "name")
    {
        std::cout << backend::name() << "\n";
        return 0;
    }
    std::cerr << "usage: driver gen <seed> <count> | run | vectors <file>... | name\n";
    return 2;
}
