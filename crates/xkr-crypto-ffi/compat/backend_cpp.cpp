// Backend over the C++ crypto sources of one hugin-native copy (the Makefile compiles this file
// once against ios/crypto and once against android/app/src/main/jni). The calls are the ones
// the app's native modules make: Core::Cryptography for the string functions (TurtleCoinModule.mm)
// and Crypto::crypto_funcs for processBlockOutputs (the processTransactionOutputs loop that
// TurtleCoinModule.mm and Java.cpp both contain, copied here).
#include "backend.h"

#include "CryptoTypes.h"
#include "StringTools.h"
#include "crypto.h"
#include "hash.h"
#include "kryptokrona.h"

#include <exception>
#include <sstream>
#include <unordered_map>

#ifndef XKR_COMPAT_COPY_NAME
#define XKR_COMPAT_COPY_NAME "cpp"
#endif

namespace backend
{
    const char *name()
    {
        return XKR_COMPAT_COPY_NAME;
    }

    /* Core::Cryptography reports some errors as C++ exceptions; here they become std::nullopt
       like any other failure, so one case never stops the run. */
    template <typename F> static Opt guarded(F f)
    {
        try
        {
            return f();
        }
        catch (const std::exception &)
        {
            return std::nullopt;
        }
    }

    bool checkKey(const Hex &publicKey)
    {
        return Core::Cryptography::checkKey(publicKey);
    }

    Opt secretKeyToPublicKey(const Hex &privateKey)
    {
        std::string out;
        if (!Core::Cryptography::secretKeyToPublicKey(privateKey, out))
        {
            return std::nullopt;
        }
        return out;
    }

    Opt scReduce32(const Hex &scalar)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::scReduce32(scalar); });
    }

    Opt cnFastHash(const Hex &data)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::cn_fast_hash(data); });
    }

    Opt cnTurtleLiteSlowHashV2(const Hex &data)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::cn_turtle_lite_slow_hash_v2(data); });
    }

    Opt hashToEllipticCurve(const Hex &hash)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::hashToEllipticCurve(hash); });
    }

    Opt hashToScalar(const Hex &data)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::hashToScalar(data); });
    }

    Opt generateKeyDerivation(const Hex &publicKey, const Hex &privateKey)
    {
        std::string out;
        if (!Core::Cryptography::generateKeyDerivation(publicKey, privateKey, out))
        {
            return std::nullopt;
        }
        return out;
    }

    Opt derivePublicKey(const Hex &derivation, uint64_t index, const Hex &publicKey)
    {
        std::string out;
        if (!Core::Cryptography::derivePublicKey(derivation, index, publicKey, out))
        {
            return std::nullopt;
        }
        return out;
    }

    Opt deriveSecretKey(const Hex &derivation, uint64_t index, const Hex &privateKey)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::deriveSecretKey(derivation, index, privateKey); });
    }

    Opt underivePublicKey(const Hex &derivation, uint64_t index, const Hex &derivedKey)
    {
        std::string out;
        if (!Core::Cryptography::underivePublicKey(derivation, index, derivedKey, out))
        {
            return std::nullopt;
        }
        return out;
    }

    Opt generateKeyImage(const Hex &publicKey, const Hex &privateKey)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::generateKeyImage(publicKey, privateKey); });
    }

    std::optional<std::pair<Hex, Hex>> generateDeterministicSubwalletKeys(const Hex &base, uint64_t index)
    {
        std::string privateKey;
        std::string publicKey;
        if (!Core::Cryptography::generateDeterministicSubwalletKeys(base, index, privateKey, publicKey))
        {
            return std::nullopt;
        }
        return std::make_pair(privateKey, publicKey);
    }

    Opt generateSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &privateKey)
    {
        return guarded([&]() -> Opt { return Core::Cryptography::generateSignature(prefixHash, publicKey, privateKey); });
    }

    bool checkSignature(const Hex &prefixHash, const Hex &publicKey, const Hex &signature)
    {
        return Core::Cryptography::checkSignature(prefixHash, publicKey, signature);
    }

    std::optional<std::vector<Hex>> generateRingSignatures(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const Hex &privateKey,
        uint64_t realIndex)
    {
        std::vector<std::string> signatures;
        if (!Core::Cryptography::generateRingSignatures(
                prefixHash, keyImage, publicKeys, privateKey, realIndex, signatures))
        {
            return std::nullopt;
        }
        return signatures;
    }

    bool checkRingSignature(
        const Hex &prefixHash,
        const Hex &keyImage,
        const std::vector<Hex> &publicKeys,
        const std::vector<Hex> &signatures)
    {
        /* The shared implementation answers false for an empty ring or a signature count
           different from the key count; only equal, non-zero counts reach the C++ call. */
        if (publicKeys.size() != signatures.size() || publicKeys.empty())
        {
            return false;
        }
        return Core::Cryptography::checkRingSignature(prefixHash, keyImage, publicKeys, signatures);
    }

    /* ---- processBlockOutputs: the loop of TurtleCoinModule.mm / Java.cpp ---- */

    struct Found
    {
        Crypto::PublicKey spendKey;
        Crypto::KeyImage keyImage;
        bool hasKeyImage;
        uint64_t amount;
        uint64_t transactionIndex;
        int64_t globalOutputIndex;
        Crypto::PublicKey key;
        std::string parentTransactionHash;
    };

    static void processTransactionOutputs(
        const Tx &tx,
        const Crypto::SecretKey &privateViewKey,
        const std::unordered_map<Crypto::PublicKey, Crypto::SecretKey> &spendKeys,
        bool isViewWallet,
        std::vector<Found> &inputs)
    {
        Crypto::PublicKey txPublicKey;
        Common::podFromHex(tx.txPublicKey, txPublicKey);

        Crypto::KeyDerivation derivation;
        /* The harness only builds blocks with valid transaction keys, so this succeeds. */
        Crypto::crypto_funcs::generate_key_derivation(txPublicKey, privateViewKey, derivation);

        uint32_t outputIndex = 0;
        for (const auto &output : tx.outputs)
        {
            Crypto::PublicKey outputKey;
            Common::podFromHex(output.key, outputKey);

            Crypto::PublicKey derivedSpendKey;
            Crypto::crypto_funcs::underive_public_key(derivation, outputIndex, outputKey, derivedSpendKey);

            const auto ourPrivateSpendKey = spendKeys.find(derivedSpendKey);
            if (ourPrivateSpendKey != spendKeys.end())
            {
                Found input;
                input.spendKey = derivedSpendKey;
                input.amount = output.amount;
                input.transactionIndex = outputIndex;
                input.globalOutputIndex = output.globalIndex;
                input.key = outputKey;
                input.parentTransactionHash = tx.hash;
                input.hasKeyImage = !isViewWallet;
                if (!isViewWallet)
                {
                    Crypto::PublicKey tmpPublicKey;
                    Crypto::SecretKey tmpSecretKey;
                    Crypto::crypto_funcs::derive_public_key(derivation, outputIndex, derivedSpendKey, tmpPublicKey);
                    Crypto::crypto_funcs::derive_secret_key(
                        derivation, outputIndex, ourPrivateSpendKey->second, tmpSecretKey);
                    Crypto::crypto_funcs::generate_key_image(tmpPublicKey, tmpSecretKey, input.keyImage);
                }
                inputs.push_back(input);
            }
            outputIndex++;
        }
    }

    static std::string jsonString(const std::string &s)
    {
        /* hashes and keys only: no characters that need escaping in the harness */
        return "\"" + s + "\"";
    }

    Opt processBlockOutputs(
        const std::optional<Tx> &coinbase,
        const std::vector<Tx> &transactions,
        const Hex &privateViewKey,
        const std::vector<SpendKey> &spendKeys,
        bool isViewWallet,
        bool processCoinbaseTransactions)
    {
        Crypto::SecretKey viewKey;
        Common::podFromHex(privateViewKey, viewKey);

        std::unordered_map<Crypto::PublicKey, Crypto::SecretKey> keys;
        for (const auto &pair : spendKeys)
        {
            Crypto::PublicKey publicKey;
            Crypto::SecretKey privateKey;
            Common::podFromHex(pair.publicKey, publicKey);
            Common::podFromHex(pair.privateKey, privateKey);
            keys[publicKey] = privateKey;
        }

        std::vector<Found> inputs;
        if (processCoinbaseTransactions && coinbase)
        {
            processTransactionOutputs(*coinbase, viewKey, keys, isViewWallet, inputs);
        }
        for (const auto &tx : transactions)
        {
            processTransactionOutputs(tx, viewKey, keys, isViewWallet, inputs);
        }

        std::ostringstream out;
        out << "[";
        for (size_t i = 0; i < inputs.size(); i++)
        {
            const auto &f = inputs[i];
            if (i > 0)
            {
                out << ",";
            }
            out << "{\"publicSpendKey\":" << jsonString(Common::podToHex(f.spendKey)) << ",\"input\":{"
                << "\"keyImage\":"
                << jsonString(f.hasKeyImage ? Common::podToHex(f.keyImage) : std::string(64, '0'))
                << ",\"amount\":" << f.amount << ",\"transactionIndex\":" << f.transactionIndex
                << ",\"globalOutputIndex\":" << f.globalOutputIndex
                << ",\"key\":" << jsonString(Common::podToHex(f.key))
                << ",\"parentTransactionHash\":" << jsonString(f.parentTransactionHash) << "}}";
        }
        out << "]";
        return out.str();
    }
} // namespace backend
