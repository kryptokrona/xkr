#!/usr/bin/env node
// Recomputes the cases written by `tests/compat_dump.rs` (xkr-crypto-ffi, Rust) with
// kryptokrona-crypto's Node addon (C++) and prints every difference.
//
//   XKR_COMPAT_DUMP=/tmp/xkr-compat.json cargo test -p xkr-crypto-ffi --test compat_dump
//   node compare.js /tmp/xkr-compat.json <kryptokrona-crypto checkout> [--allow-js]
//
// The second argument is a built checkout of kryptokrona-crypto: 1.x (dist/ and
// build/Release/turtlecoin-crypto.node, the C++ addon: the reference) or 2.x (dist/ and
// prebuilt/napi/*.node, the Node-API addon over this crate: checks the binding). Without
// --allow-js the script refuses to run on the asm.js fallback of 1.x.
//
// Exit code 0: every case agrees. 1: differences. 2: usage or loading problem.
'use strict';

const fs = require('fs');
const path = require('path');

function usage(message) {
    if (message) {
        console.error(message);
    }
    console.error('usage: node compare.js <cases.json> <kryptokrona-crypto dir> [--allow-js]');
    process.exit(2);
}

const args = process.argv.slice(2);
const allowJs = args.includes('--allow-js');
const positional = args.filter((a) => !a.startsWith('--'));
if (positional.length !== 2) {
    usage();
}
const [casesFile, cryptoDir] = positional;

let doc;
try {
    doc = JSON.parse(fs.readFileSync(casesFile, 'utf8'));
} catch (e) {
    usage('cannot read ' + casesFile + ': ' + e.message);
}

let lib;
try {
    lib = require(path.resolve(cryptoDir, 'dist', 'index.js'));
} catch (e) {
    usage('cannot load kryptokrona-crypto from ' + cryptoDir + ' (run its build first): ' + e.message);
}

const crypto = new lib.Crypto();
if (!crypto.isReady) {
    usage('kryptokrona-crypto did not initialise a backend');
}
const backend = lib.CryptoType[crypto.type];
console.log('kryptokrona-crypto backend: ' + backend);
/* NODEADDON: the C++ addon of kryptokrona-crypto 1.x (the reference). NAPI: the Node-API addon
   of 2.x, which runs this crate; the comparison then checks the binding (names, argument and
   result shapes). */
const nativeTypes = [lib.CryptoType.NODEADDON, lib.CryptoType.NAPI].filter((t) => t !== undefined);
if (!nativeTypes.includes(crypto.type) && !allowJs) {
    usage('no native addon is loaded (backend ' + backend + '); build it or pass --allow-js');
}

/* Results of the Crypto methods in the shape the Rust side wrote them */
function normalise(fn, value) {
    if (value && typeof value === 'object' && !Array.isArray(value)) {
        if (fn === 'clsagSign') {
            return { c1: value.c1, s: value.s };
        }
        return { private_key: value.private_key, public_key: value.public_key };
    }
    return value;
}

function same(a, b) {
    return JSON.stringify(a) === JSON.stringify(b);
}

/* Key pairs whose reference result has no public key (the 1.x addon names the field
   `publicKey`, which the TypeScript wrapper of those versions does not read for every
   function): the private keys are compared, and the reference public key is computed from the
   private key with the reference's own secretKeyToPublicKey. */
async function completeKeyPair(node) {
    if (node && typeof node === 'object' && 'private_key' in node && !node.public_key && node.private_key) {
        derivedPublicKeys++;
        return { private_key: node.private_key, public_key: await crypto.secretKeyToPublicKey(node.private_key) };
    }
    return node;
}

let derivedPublicKeys = 0;

async function main() {
    const counts = {};
    const differences = [];
    for (const c of doc.cases) {
        counts[c.fn] = (counts[c.fn] || 0) + 1;
        if (typeof crypto[c.fn] !== 'function') {
            differences.push({ fn: c.fn, args: c.args, rust: c.result, node: 'missing method' });
            continue;
        }
        let node;
        try {
            node = await completeKeyPair(normalise(c.fn, await crypto[c.fn](...c.args)));
        } catch (e) {
            node = { error: e.message };
        }
        if (!same(node, c.result)) {
            differences.push({ fn: c.fn, args: c.args, rust: c.result, node });
        }
    }

    const fns = Object.keys(counts).sort();
    console.log(doc.cases.length + ' cases (seed ' + doc.seed + ') over ' + fns.length + ' functions');
    if (derivedPublicKeys > 0) {
        console.log('  (' + derivedPublicKeys + ' reference public keys computed with secretKeyToPublicKey)');
    }
    for (const fn of fns) {
        const bad = differences.filter((d) => d.fn === fn).length;
        console.log('  ' + (bad === 0 ? 'ok  ' : 'DIFF') + ' ' + fn + ' ' + (counts[fn] - bad) + '/' + counts[fn]);
    }
    for (const d of differences.slice(0, 20)) {
        console.log(JSON.stringify(d));
    }
    if (differences.length > 20) {
        console.log('... ' + (differences.length - 20) + ' more');
    }
    process.exit(differences.length === 0 ? 0 : 1);
}

main().catch((e) => {
    console.error(e);
    process.exit(2);
});
