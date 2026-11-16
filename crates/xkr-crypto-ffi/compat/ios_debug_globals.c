/* The iOS copy's slow-hash-common.h declares these per-thread debug buffers; the app defines
   them in TurtleCoinModule.mm, which the harness does not compile. Linked into the iOS
   builds only. */
__thread char g_debug_stage_info[20][256];
__thread int g_debug_stage_count = 0;
__thread int g_debug_hash_active = 0;
