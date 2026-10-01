// The CPU models Bochs's cpu/init.cc knows. Our build finds this file before
// Bochs's own (softcpu/build.rs puts softcpu/include first): it adds qemu64
// as model 0, the one the software CPU selects, then lists Bochs's models
// unchanged. Included several times with different definitions of
// bx_define_cpudb, so no include guard.
bx_define_cpudb(qemu64)
#include "../bochs/bochs/cpudb.h"
