// Differential harness: one interface, three implementations.
//
//   backend_cpp.cpp   the C++ crypto sources of a hugin-native copy (ios/crypto or
//                     android/app/src/main/jni), called the way the app's native modules call
//                     them (Core::Cryptography and Crypto::crypto_funcs);
//   backend_rust.cpp  the Rust core through the C ABI (include/xkr_crypto_ffi.h).
//
// Everything is hex in, lowercase hex out; std::nullopt / false where the implementation
// reports a failure. driver.cpp is the same for every backend.
#pragma once

#include <cstdint>
#include <optional>
#include <string>
#include <utility>
#include <vector>

namespace backend
{
    using Hex = std::string;
    using Opt = std::optional<std::string>;

    struct Output
    {
        Hex key;
        uint64_t amount;
        int64_t globalIndex;
    };

    struct Tx
    {
        std::vector<Output> outputs;
        std::string hash;
        Hex txPublicKey;
    };

    struct SpendKey
    {
        Hex publicKey;
        Hex privateKey;
    };

    const char *name();

    bool checkKey(const Hex &publicKey);
    Opt secretKeyToPublicKey(const Hex &privateKey);
    Opt scReduce32(const Hex &scalar);
    Opt cnFastHash(const Hex &data);
    Opt cnTurtleLiteSlowHashV2(const Hex &data);
    Opt hashToEllipticCurve(const Hex &hash);
    Opt hashToScalar(const Hex &data);

    Opt generateKeyDerivation(const Hex &publicKey, const Hex &privateKey);
    Opt derivePublicKey(const Hex &derivation, uint64_t index, const Hex &publicKey);
    Opt deriveSecretKey(const Hex &derivation, uint64_t index, const Hex &privateKey);
    Opt underivePublicKey(const Hex &derivation, uint64_t index, const Hex &derivedKey);
    Opt generateKeyImage(const Hex &publicKey, const Hex &privateKey);
    /* (private, public) */
    std::optional<std::pair<Hex, Hex>> generateDeterministicSubwalletKeys(const Hex &base, uint64_t index);

    Opt generateSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &privateKey);
    bool checkSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &signature);
    std::optional<std::vector<Hex>> generateRingSignatures(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const Hex &privateKey,
        uint64_t realIndex);
    bool checkRingSignature(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const std::vector<Hex> &signatures);

    /* The JSON text processBlockOutputs resolves with (serde_json's compact form and key
       order, which the C++ backend reproduces), or std::nullopt on failure. View wallets get
       the all-zero key image in every backend (the rule of the shared implementation). */
    Opt processBlockOutputs(
        const std::optional<Tx> &coinbase,
        const std::vector<Tx> &transactions,
        const Hex &privateViewKey,
        const std::vector<SpendKey> &spendKeys,
        bool isViewWallet,
        bool processCoinbaseTransactions);
} // namespace backend
