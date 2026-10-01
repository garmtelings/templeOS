// The CPU model of QEMU's `-cpu qemu64`, which the hardware-virtualized
// machine also presents: CPUID comes from the host (the same table, checked
// against QEMU captures, see vmm/src/cpuid.rs), and the instruction set
// Bochs enables follows the features that table reports.

#include "bochs.h"
#include "cpu/cpu.h"
#include "cpu/cpuid.h"

#include "softcpu.h"

extern softcpu_host *softcpu_current_host(void);

class qemu64_t : public bx_cpuid_t {
public:
  qemu64_t(BX_CPU_C *cpu): bx_cpuid_t(cpu)
  {
    // CPUID.1:EDX FPU DE PSE TSC MSR PAE MCE CX8 APIC SEP MTRR PGE MCA CMOV
    // PAT PSE36 CLFSH MMX FXSR SSE SSE2; CPUID.1:ECX SSE3 CX16;
    // CPUID.80000001h:EDX SYSCALL NX LM, ECX LAHF_LM (and SVM, which this
    // build doesn't emulate and neither SeaBIOS nor TempleOS uses).
    enable_cpu_extension(BX_ISA_X87);
    enable_cpu_extension(BX_ISA_486);
    enable_cpu_extension(BX_ISA_PENTIUM);
    enable_cpu_extension(BX_ISA_P6);
    enable_cpu_extension(BX_ISA_MMX);
    enable_cpu_extension(BX_ISA_DEBUG_EXTENSIONS);
    enable_cpu_extension(BX_ISA_PSE);
    enable_cpu_extension(BX_ISA_PAE);
    enable_cpu_extension(BX_ISA_PGE);
    enable_cpu_extension(BX_ISA_MTRR);
    enable_cpu_extension(BX_ISA_PAT);
    enable_cpu_extension(BX_ISA_SYSENTER_SYSEXIT);
    enable_cpu_extension(BX_ISA_SYSCALL_SYSRET_LEGACY);
    enable_cpu_extension(BX_ISA_CLFLUSH);
    enable_cpu_extension(BX_ISA_SSE);
    enable_cpu_extension(BX_ISA_SSE2);
    enable_cpu_extension(BX_ISA_SSE3);
    enable_cpu_extension(BX_ISA_CMPXCHG16B);
    enable_cpu_extension(BX_ISA_XAPIC);
    enable_cpu_extension(BX_ISA_LONG_MODE);
    enable_cpu_extension(BX_ISA_LM_LAHF_SAHF);
    enable_cpu_extension(BX_ISA_NX);
  }
  virtual ~qemu64_t() {}

  virtual const char *get_name(void) const { return "qemu64"; }

  virtual void get_cpuid_leaf(Bit32u function, Bit32u subfunction, cpuid_function_t *leaf) const
  {
    uint32_t r[4] = {0, 0, 0, 0};
    softcpu_host *h = softcpu_current_host();
    h->cpuid(h->ctx, cpu->bx_cpuid, function, subfunction, r);
    leaf->eax = r[0];
    leaf->ebx = r[1];
    leaf->ecx = r[2];
    leaf->edx = r[3];
  }

  virtual void dump_cpuid(void) const {}
};

bx_cpuid_t *create_qemu64_cpuid(BX_CPU_C *cpu) { return new qemu64_t(cpu); }
