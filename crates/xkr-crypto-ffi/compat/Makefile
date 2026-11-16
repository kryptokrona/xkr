# Differential harness: the two C++ crypto copies of hugin-native against the Rust core
# (see ../README.md). Host build, no device needed.
#
#   make rust            build the Rust staticlib (cargo, through the optional CAP wrapper)
#   make -j3 drivers     build the five drivers (C++ copies x AES/portable, Rust)
#   ./run.sh [seed] [n]  everything, with the comparisons
#
# Drivers (build/driver-<name>):
#   ios-aes, ios-portable          hugin-native ios/crypto            (iOS copy)
#   android-aes, android-portable  hugin-native android/.../jni       (Android copy)
#   rust                           crates/xkr-crypto-ffi via its C ABI
# "aes" is the hardware AES slow-hash path of the host (slow-hash-x86.c with AES-NI on x86_64,
# slow-hash-arm.c with the crypto extension on aarch64); "portable" is -DNO_AES
# (slow-hash-portable.c, what the iOS simulator build uses).
#
# Cross build for the ARM slow-hash files (the ones that differ between the copies), e.g.
#   make rust CROSS=aarch64-linux-gnu-  &&  make -j3 drivers CROSS=aarch64-linux-gnu-
#   RUN="qemu-aarch64 -L /usr/aarch64-linux-gnu" ./run.sh

HUGIN       ?= $(abspath ../../../../hugin-native)
IOS_DIR     ?= $(HUGIN)/ios/crypto
ANDROID_DIR ?= $(HUGIN)/android/app/src/main/jni
REPO        := $(abspath ../../..)
# optional wrapper for heavy steps (e.g. CAP="nice -n 19"); empty by default
CAP         ?=
CROSS       ?=

CC  = $(CROSS)gcc
CXX = $(CROSS)g++

MACHINE := $(shell $(CC) -dumpmachine)
ifneq (,$(findstring aarch64,$(MACHINE)))
AES_FLAGS   := -march=armv8-a+crypto
RUST_TRIPLE ?= aarch64-unknown-linux-gnu
else
AES_FLAGS   := -maes -msse2
RUST_TRIPLE ?=
endif

ifeq ($(CROSS),)
RUST_LIB    ?= $(REPO)/target/release/libxkr_crypto_ffi.a
RUST_TARGET :=
else
RUST_LIB    ?= $(REPO)/target/$(RUST_TRIPLE)/release/libxkr_crypto_ffi.a
RUST_TARGET := --target $(RUST_TRIPLE)
endif
RUST_LIBS := -lgcc_s -lutil -lrt -lpthread -lm -ldl

BUILD := build

# Release-like flags of both apps: NDEBUG (the C++ asserts are compiled out in release),
# -fno-strict-aliasing and FORCE_USE_HEAP as in the iOS podspec. -w: third-party sources.
COMMON   := -O2 -DNDEBUG -fno-strict-aliasing -DFORCE_USE_HEAP -w
CFLAGS   := -std=gnu11 $(COMMON) -idirafter shim
# the copies rely on <cstring> arriving through other headers (as on Apple and Android libc++)
CXXFLAGS := -std=c++17 -fexceptions -include cstring -include cstdint $(COMMON)
aes_FLAGS      := $(AES_FLAGS)
portable_FLAGS := -DNO_AES

CRYPTO_CPP := crypto.cpp crypto-ops.cpp crypto-ops-data.cpp hash.cpp keccak.cpp kryptokrona.cpp \
              multisig.cpp StringTools.cpp
CRYPTO_C   := aesb.c blake256.c groestl.c hash-extra-blake.c hash-extra-groestl.c hash-extra-jh.c \
              hash-extra-skein.c jh.c oaes_lib.c skein.c slow-hash-arm.c slow-hash-portable.c \
              slow-hash-x86.c tree-hash.c

DRIVERS := $(BUILD)/driver-ios-aes $(BUILD)/driver-ios-portable \
           $(BUILD)/driver-android-aes $(BUILD)/driver-android-portable $(BUILD)/driver-rust

.PHONY: drivers rust clean
drivers: $(DRIVERS)

# $(1) copy name, $(2) variant, $(3) source directory, $(4) extra objects
define copy_rules
$(BUILD)/$(1)-$(2):
	mkdir -p $$@
$(BUILD)/$(1)-$(2)/%.o: $(3)/%.c | $(BUILD)/$(1)-$(2)
	$$(CC) $$(CFLAGS) $$($(2)_FLAGS) -I$(3) -c $$< -o $$@
$(BUILD)/$(1)-$(2)/%.o: $(3)/%.cpp | $(BUILD)/$(1)-$(2)
	$$(CXX) $$(CXXFLAGS) $$($(2)_FLAGS) -I$(3) -c $$< -o $$@
$(BUILD)/$(1)-$(2)/backend_cpp.o: backend_cpp.cpp backend.h | $(BUILD)/$(1)-$(2)
	$$(CXX) $$(CXXFLAGS) $$($(2)_FLAGS) -I$(3) -I. '-DXKR_COMPAT_COPY_NAME="$(1)-$(2)"' -c $$< -o $$@
$(BUILD)/$(1)-$(2)/driver.o: driver.cpp backend.h | $(BUILD)/$(1)-$(2)
	$$(CXX) $$(CXXFLAGS) -I. -c $$< -o $$@
$(BUILD)/$(1)-$(2)/ios_debug_globals.o: ios_debug_globals.c | $(BUILD)/$(1)-$(2)
	$$(CC) $$(CFLAGS) -c $$< -o $$@
$(BUILD)/driver-$(1)-$(2): $(addprefix $(BUILD)/$(1)-$(2)/,$(CRYPTO_CPP:.cpp=.o) $(CRYPTO_C:.c=.o) backend_cpp.o driver.o $(4))
	$$(CXX) $$^ -lpthread -o $$@
endef

$(eval $(call copy_rules,ios,aes,$(IOS_DIR),ios_debug_globals.o))
$(eval $(call copy_rules,ios,portable,$(IOS_DIR),ios_debug_globals.o))
$(eval $(call copy_rules,android,aes,$(ANDROID_DIR),))
$(eval $(call copy_rules,android,portable,$(ANDROID_DIR),))

$(BUILD)/rust:
	mkdir -p $@
$(BUILD)/rust/%.o: %.cpp backend.h ../include/xkr_crypto_ffi.h | $(BUILD)/rust
	$(CXX) $(CXXFLAGS) -I. -I../include -c $< -o $@
$(BUILD)/driver-rust: $(BUILD)/rust/driver.o $(BUILD)/rust/backend_rust.o $(RUST_LIB)
	$(CXX) $(BUILD)/rust/driver.o $(BUILD)/rust/backend_rust.o $(RUST_LIB) $(RUST_LIBS) -o $@

$(RUST_LIB):
	@echo "missing $(RUST_LIB): run 'make rust$(if $(CROSS), CROSS=$(CROSS))' first" >&2
	@false

# The only cargo invocation; run it alone (never in parallel with the C++ build).
rust:
	cd $(REPO) && $(CAP) cargo build -j 3 --release -p xkr-crypto-ffi $(RUST_TARGET)

clean:
	rm -rf $(BUILD)
