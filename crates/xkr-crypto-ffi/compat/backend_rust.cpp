// Backend over the Rust core through its C ABI (the same calls the React Native C++ TurboModule
// makes).
#include "backend.h"

#include "xkr_crypto_ffi.h"

#include <cstdio>
#include <cstdlib>
#include <sstream>

namespace backend
{
    const char *name()
    {
        return "rust";
    }

    static XkrCryptoStr str(const std::string &s)
    {
        return XkrCryptoStr {s.data(), s.size()};
    }

    static std::vector<XkrCryptoStr> strs(const std::vector<std::string> &items)
    {
        std::vector<XkrCryptoStr> out;
        out.reserve(items.size());
        for (const auto &s : items)
        {
            out.push_back(str(s));
        }
        return out;
    }

    static std::string take(XkrCryptoBuffer buffer)
    {
        std::string out(reinterpret_cast<const char *>(buffer.ptr), buffer.len);
        xkr_crypto_buffer_free(buffer);
        return out;
    }

    /* Fixed-size hex result of a call `f(out, cap)`. */
    template <typename F> static Opt hexOut(F f)
    {
        char out[XKR_CRYPTO_HEX64_SIZE] = {0};
        if (f(out, sizeof(out)) != XKR_CRYPTO_OK)
        {
            return std::nullopt;
        }
        return std::string(out);
    }

    /* Callers compute `status` in a statement of its own before passing `value`: C++ leaves the
       evaluation order of function arguments unspecified, so `flag(call(&v), v)` may read `v`
       before the call writes it. */
    static bool flag(int32_t status, uint8_t value)
    {
        if (status != XKR_CRYPTO_OK)
        {
            std::fprintf(stderr, "rust backend: %s\n", xkr_crypto_status_message(status));
            std::abort();
        }
        return value != 0;
    }

    bool checkKey(const Hex &publicKey)
    {
        uint8_t valid = 0;
        const int32_t status = xkr_crypto_check_key(str(publicKey), &valid);
        return flag(status, valid);
    }

    Opt secretKeyToPublicKey(const Hex &privateKey)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_secret_key_to_public_key(str(privateKey), o, c); });
    }

    Opt scReduce32(const Hex &scalar)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_sc_reduce32(str(scalar), o, c); });
    }

    Opt cnFastHash(const Hex &data)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_cn_fast_hash(str(data), o, c); });
    }

    Opt cnTurtleLiteSlowHashV2(const Hex &data)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_cn_turtle_lite_slow_hash_v2(str(data), o, c); });
    }

    Opt hashToEllipticCurve(const Hex &hash)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_hash_to_elliptic_curve(str(hash), o, c); });
    }

    Opt hashToScalar(const Hex &data)
    {
        return hexOut([&](char *o, size_t c) { return xkr_crypto_hash_to_scalar(str(data), o, c); });
    }

    Opt generateKeyDerivation(const Hex &publicKey, const Hex &privateKey)
    {
        return hexOut([&](char *o, size_t c)
                   { return xkr_crypto_generate_key_derivation(str(publicKey), str(privateKey), o, c); });
    }

    Opt derivePublicKey(const Hex &derivation, uint64_t index, const Hex &publicKey)
    {
        return hexOut(
            [&](char *o, size_t c)
            { return xkr_crypto_derive_public_key(str(derivation), static_cast<double>(index), str(publicKey), o, c); });
    }

    Opt deriveSecretKey(const Hex &derivation, uint64_t index, const Hex &privateKey)
    {
        return hexOut(
            [&](char *o, size_t c)
            { return xkr_crypto_derive_secret_key(str(derivation), static_cast<double>(index), str(privateKey), o, c); });
    }

    Opt underivePublicKey(const Hex &derivation, uint64_t index, const Hex &derivedKey)
    {
        return hexOut(
            [&](char *o, size_t c)
            { return xkr_crypto_underive_public_key(str(derivation), static_cast<double>(index), str(derivedKey), o, c); });
    }

    Opt generateKeyImage(const Hex &publicKey, const Hex &privateKey)
    {
        return hexOut([&](char *o, size_t c)
                   { return xkr_crypto_generate_key_image(str(publicKey), str(privateKey), o, c); });
    }

    std::optional<std::pair<Hex, Hex>> generateDeterministicSubwalletKeys(const Hex &base, uint64_t index)
    {
        char publicKey[XKR_CRYPTO_HEX32_SIZE] = {0};
        char privateKey[XKR_CRYPTO_HEX32_SIZE] = {0};
        if (xkr_crypto_generate_deterministic_subwallet_keys(
                str(base), static_cast<double>(index), publicKey, sizeof(publicKey), privateKey, sizeof(privateKey))
            != XKR_CRYPTO_OK)
        {
            return std::nullopt;
        }
        return std::make_pair(std::string(privateKey), std::string(publicKey));
    }

    Opt generateSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &privateKey)
    {
        return hexOut([&](char *o, size_t c)
                   { return xkr_crypto_generate_signature(str(prefixHash), str(publicKey), str(privateKey), o, c); });
    }

    bool checkSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &signature)
    {
        uint8_t valid = 0;
        const int32_t status =
            xkr_crypto_check_signature(str(prefixHash), str(publicKey), str(signature), &valid);
        return flag(status, valid);
    }

    std::optional<std::vector<Hex>> generateRingSignatures(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const Hex &privateKey,
        uint64_t realIndex)
    {
        const auto keys = strs(publicKeys);
        XkrCryptoBuffer buffer {nullptr, 0};
        if (xkr_crypto_generate_ring_signatures(
                str(prefixHash),
                str(keyImage),
                keys.data(),
                keys.size(),
                str(privateKey),
                static_cast<double>(realIndex),
                &buffer)
            != XKR_CRYPTO_OK)
        {
            return std::nullopt;
        }
        const std::string joined = take(buffer);
        std::vector<Hex> out;
        for (size_t i = 0; i + 128 <= joined.size(); i += 128)
        {
            out.push_back(joined.substr(i, 128));
        }
        return out;
    }

    bool checkRingSignature(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const std::vector<Hex> &signatures)
    {
        const auto keys = strs(publicKeys);
        const auto sigs = strs(signatures);
        uint8_t valid = 0;
        const int32_t status = xkr_crypto_check_ring_signature(
            str(prefixHash), str(keyImage), keys.data(), keys.size(), sigs.data(), sigs.size(), &valid);
        return flag(status, valid);
    }

    static void txJson(std::ostringstream &out, const Tx &tx)
    {
        out << "{\"keyOutputs\":[";
        for (size_t i = 0; i < tx.outputs.size(); i++)
        {
            const auto &o = tx.outputs[i];
            out << (i > 0 ? "," : "") << "{\"key\":\"" << o.key << "\",\"amount\":" << o.amount
                << ",\"globalIndex\":" << o.globalIndex << "}";
        }
        out << "],\"hash\":\"" << tx.hash << "\",\"transactionPublicKey\":\"" << tx.txPublicKey << "\"}";
    }

    Opt processBlockOutputs(
        const std::optional<Tx> &coinbase,
        const std::vector<Tx> &transactions,
        const Hex &privateViewKey,
        const std::vector<SpendKey> &spendKeys,
        bool isViewWallet,
        bool processCoinbaseTransactions)
    {
        std::ostringstream block;
        block << "{";
        if (coinbase)
        {
            block << "\"coinbaseTransaction\":";
            txJson(block, *coinbase);
            block << ",";
        }
        block << "\"transactions\":[";
        for (size_t i = 0; i < transactions.size(); i++)
        {
            if (i > 0)
            {
                block << ",";
            }
            txJson(block, transactions[i]);
        }
        block << "]}";

        std::ostringstream keys;
        keys << "[";
        for (size_t i = 0; i < spendKeys.size(); i++)
        {
            keys << (i > 0 ? "," : "") << "{\"publicKey\":\"" << spendKeys[i].publicKey << "\",\"privateKey\":\""
                 << spendKeys[i].privateKey << "\"}";
        }
        keys << "]";

        const std::string blockJson = block.str();
        const std::string keysJson = keys.str();
        XkrCryptoBuffer buffer {nullptr, 0};
        if (xkr_crypto_process_block_outputs(
                str(blockJson),
                str(privateViewKey),
                str(keysJson),
                isViewWallet ? 1 : 0,
                processCoinbaseTransactions ? 1 : 0,
                &buffer)
            != XKR_CRYPTO_OK)
        {
            return std::nullopt;
        }
        return take(buffer);
    }
} // namespace backend
