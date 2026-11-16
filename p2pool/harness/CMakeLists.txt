# p2pool_vectors: the C++ side of the Rust p2pool differential tests (see p2pool_vectors.cpp).
#
# Configured from <p2pool checkout>/tools/vectors with the sources of the unit-test build
# (tests/CMakeLists.txt) minus googletest and the test files, plus p2pool_vectors.cpp. Always -O2:
# reference builds use no higher optimisation level, and no -ffast-math.
cmake_minimum_required(VERSION 3.10)
project(p2pool_vectors C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_BUILD_TYPE "" CACHE STRING "" FORCE)
set(CMAKE_C_FLAGS "${CMAKE_C_FLAGS} -pthread -O2")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -pthread -O2")

set(ROOT ${CMAKE_CURRENT_SOURCE_DIR}/../..)

# The CryptoNight-Turtle library brings its own -O2 -fno-strict-aliasing.
add_subdirectory(${ROOT}/external/src/cryptonight cryptonight)

add_definitions(-DWITH_MERGE_MINING_DONATION)
add_definitions(-DP2POOL_UNIT_TESTS)
add_definitions(-DP2POOL_LOG_DISABLE)
add_definitions(-DWITH_INDEXED_HASHES)
add_definitions(-DHAVE_BUILTIN_CLZLL)
add_definitions("-DRAPIDJSON_PARSE_DEFAULT_FLAGS=kParseTrailingCommasFlag")

set(SOURCES
	p2pool_vectors.cpp
	${ROOT}/external/src/cryptonote/crypto-ops-data.c
	${ROOT}/external/src/cryptonote/crypto-ops.c
	${ROOT}/external/src/cryptonote/fcmp_pp_crypto.cpp
	${ROOT}/external/src/hardforks/hardforks.cpp
	${ROOT}/external/src/siphash/siphash.c
	${ROOT}/src/block_cache.cpp
	${ROOT}/src/block_template.cpp
	${ROOT}/src/crypto.cpp
	${ROOT}/src/keccak.cpp
	${ROOT}/src/log.cpp
	${ROOT}/src/memory_leak_debug.cpp
	${ROOT}/src/mempool.cpp
	${ROOT}/src/merkle.cpp
	${ROOT}/src/p2p_server.cpp
	${ROOT}/src/p2pool.cpp
	${ROOT}/src/p2pool_api.cpp
	${ROOT}/src/params.cpp
	${ROOT}/src/pool_block.cpp
	${ROOT}/src/pow_hash.cpp
	${ROOT}/src/side_chain.cpp
	${ROOT}/src/stratum_server.cpp
	${ROOT}/src/tcp_server.cpp
	${ROOT}/src/thread_pool.cpp
	${ROOT}/src/util.cpp
	${ROOT}/src/wallet.cpp
	${ROOT}/src/i2p.cpp
	${ROOT}/src/indexed_hash.cpp
)

set(BORINGSSL_DIR ${ROOT}/external/src/grpc/third_party/boringssl-with-bazel)
set(ED25519_SOURCES
	${BORINGSSL_DIR}/crypto/curve25519/curve25519.cc
	${BORINGSSL_DIR}/crypto/sha/sha256.cc
	${BORINGSSL_DIR}/crypto/sha/sha512.cc
	${ROOT}/external/src/crypto/bssl_stub.cpp
)
set_source_files_properties(${ED25519_SOURCES} PROPERTIES COMPILE_FLAGS "-w -DOPENSSL_NO_ASM -I${BORINGSSL_DIR} -I${BORINGSSL_DIR}/include")
set(SOURCES ${SOURCES} ${ED25519_SOURCES})

if (CMAKE_SYSTEM_PROCESSOR MATCHES "x86_64|AMD64|amd64")
	set(SOURCES ${SOURCES} ${ROOT}/src/keccak_bmi.cpp)
	set_source_files_properties(${ROOT}/src/keccak_bmi.cpp PROPERTIES COMPILE_FLAGS "-mbmi")
endif()

find_path(UV_INCLUDE_DIR NAMES uv.h PATH_SUFFIXES "include")
find_library(UV_LIBRARY NAMES uv libuv.a)

include_directories(${BORINGSSL_DIR})
include_directories(${BORINGSSL_DIR}/include)
include_directories(${ROOT}/src)
include_directories(${ROOT}/external/src)
include_directories(${ROOT}/external/src/crypto)
include_directories(${ROOT}/external/src/cryptonote)
include_directories(${UV_INCLUDE_DIR})
include_directories(${ROOT}/external/src/protobuf-c)
include_directories(${ROOT}/external/src/RandomX/src)
include_directories(${ROOT}/external/src/rapidjson/include)
include_directories(${ROOT}/external/src/robin-hood-hashing/src/include)

add_executable(p2pool_vectors ${SOURCES})
target_link_libraries(p2pool_vectors ${UV_LIBRARY} cryptonight pthread)
