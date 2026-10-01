// What the Bochs CPU core needs from the rest of Bochs, provided for
// TempleOS.exe: memory (RAM, the BIOS, and device accesses forwarded to the
// host), port I/O and the 8259's interrupt acknowledge (forwarded too), the
// configuration parameters the CPU reads, logging, and the run loop with
// the C interface in softcpu.h.
//
// Bochs's own pc_system.cc (timers and tick counting) and gui/paramtree.cc
// are compiled unchanged alongside; everything else of Bochs outside cpu/ is
// replaced by this file.

#include <map>
#include <string>
#include <cstdarg>
#include <cstdio>
#include <cstring>
#include <csetjmp>

#include "bochs.h"
#include "cpu/cpu.h"
#include "iodev/iodev.h"
#include "gui/siminterface.h"
#include "gui/paramtree.h"
#include "pc_system.h"
#include "cpu/icache.h"

#include "softcpu.h"

// ---------------------------------------------------------------------------
// The host and the machine's memory.

static softcpu_host host;
static bool host_set;

static Bit8u *ram;
static Bit64u ram_size;
static const Bit8u *bios;
static Bit64u bios_base;  // the BIOS occupies bios_base .. 4 GiB

static std::string error_text;

softcpu_host *softcpu_current_host(void) { return &host; }

static void stop_all(void);

// Bochs's report of something it can't go on with: remember the first.
static void fatal_error(const std::string &text)
{
  if (error_text.empty()) error_text = text;
  stop_all();
}

static void host_log(int level, const char *text)
{
  if (host_set && host.log) host.log(host.ctx, level, text);
}

// ---------------------------------------------------------------------------
// Globals the CPU core refers to (Bochs defines them in main.cc and friends).

bx_pc_system_c bx_pc_system;
bx_debug_t bx_dbg;
Bit8u bx_cpu_count;
BX_CPU_C **bx_cpu_array;
Bit32u apic_id_mask;
bool simulate_xapic;
bx_gui_c *bx_gui = NULL;
bool bx_user_quit;
bx_list_c *root_param;
logfunctions *pluginlog;
logfunc_t *genlog;
logfunctions *siminterface_log;
bx_simulator_interface_c *SIM;

void print_statistics_tree(bx_param_c *node, int level) {}
int bx_atexit(void) { return 0; }
void bx_gui_c::cleanup(void) {}

// ---------------------------------------------------------------------------
// Logging: Bochs's logfunctions, reduced to forwarding messages to the host.
// Panics and fatal errors stop the machine.

int logfunctions::default_onoff[N_LOGLEV];

logfunctions::logfunctions(void) : name(NULL), prefix(NULL), logio(NULL) {}
logfunctions::~logfunctions(void)
{
  free(name);
  free(prefix);
}

void logfunctions::put(const char *p) { put(p, p); }

void logfunctions::put(const char *n, const char *p)
{
  free(name);
  free(prefix);
  name = strdup(n);
  prefix = strdup(p);
}

static std::string format(logfunctions *lf, const char *fmt, va_list ap)
{
  char buf[1024];
  vsnprintf(buf, sizeof buf, fmt, ap);
  const char *prefix = lf && lf->getprefix() ? lf->getprefix() : "bochs";
  return std::string("[") + prefix + "] " + buf;
}

#define LOG_BODY(level)                              \
  va_list ap;                                        \
  va_start(ap, fmt);                                 \
  std::string text = format(this, fmt, ap);          \
  va_end(ap);                                        \
  host_log(level, text.c_str());

void logfunctions::ldebug(const char *fmt, ...) { LOG_BODY(0) }
void logfunctions::info(const char *fmt, ...) { LOG_BODY(1) }
void logfunctions::lwarn(const char *fmt, ...) { LOG_BODY(1) }
void logfunctions::error(const char *fmt, ...) { LOG_BODY(2) }
void logfunctions::panic(const char *fmt, ...) { LOG_BODY(3) fatal_error(text); }
void logfunctions::fatal1(const char *fmt, ...) { LOG_BODY(3) fatal_error(text); }

static logfunctions softcpu_log;

// ---------------------------------------------------------------------------
// Configuration: the parameters the CPU core reads, as a bochsrc would set
// them for this machine.

class softcpu_sim_c : public bx_simulator_interface_c {
  std::map<std::string, bx_param_c *> params;

  template <class T> T *get(const char *pname)
  {
    auto it = params.find(pname);
    if (it == params.end()) {
      fatal_error(std::string("[softcpu] unknown parameter ") + pname);
      return NULL;
    }
    return dynamic_cast<T *>(it->second);
  }

public:
  void add(const char *pname, bx_param_c *p) { params[pname] = p; }

  bx_param_num_c *num(const char *pname) { return get<bx_param_num_c>(pname); }

  virtual bx_param_c *get_param(const char *pname, bx_param_c *base) { return get<bx_param_c>(pname); }
  virtual bx_param_num_c *get_param_num(const char *pname, bx_param_c *base) { return get<bx_param_num_c>(pname); }
  virtual bx_param_string_c *get_param_string(const char *pname, bx_param_c *base) { return get<bx_param_string_c>(pname); }
  virtual bx_param_bool_c *get_param_bool(const char *pname, bx_param_c *base) { return get<bx_param_bool_c>(pname); }
  virtual bx_param_enum_c *get_param_enum(const char *pname, bx_param_c *base) { return get<bx_param_enum_c>(pname); }
};

static softcpu_sim_c *sim;

static void init_params(void)
{
  if (sim) return;
  sim = new softcpu_sim_c;
  SIM = sim;
  static const char *models[] = { "qemu64", NULL };            // index 0: our model (cpudb.h)
  static const char *freq[] = { "hardware", "none", "ips", NULL };
  sim->add(BXPN_CPU_MODEL, new bx_param_enum_c(NULL, "model", "", "", models, 0, 0));
  sim->add(BXPN_CPUID_FREQ, new bx_param_enum_c(NULL, "cpuid_freq", "", "", freq, 0, 0));
  // One tick per instruction; Bochs converts ticks to time with this.
  sim->add(BXPN_IPS, new bx_param_num_c(NULL, "ips", "", "", 1, BX_MAX_BIT32U, 1000000000));
  sim->add(BXPN_SMP_QUANTUM, new bx_param_num_c(NULL, "quantum", "", "", 1, 32, 16));
  sim->add(BXPN_CPU_NPROCESSORS, new bx_param_num_c(NULL, "n_processors", "", "", 1, 255, 1));
  sim->add(BXPN_CPU_NCORES, new bx_param_num_c(NULL, "n_cores", "", "", 1, 255, 1));
  sim->add(BXPN_CPU_NTHREADS, new bx_param_num_c(NULL, "n_threads", "", "", 1, 255, 1));
  // A triple fault resets the machine, as on QEMU and real hardware.
  sim->add(BXPN_RESET_ON_TRIPLE_FAULT, new bx_param_bool_c(NULL, "reset_on_triple_fault", "", "", 1));
  sim->add(BXPN_CPUID_LIMIT_WINNT, new bx_param_bool_c(NULL, "cpuid_limit_winnt", "", "", 0));
  sim->add(BXPN_MWAIT_IS_NOP, new bx_param_bool_c(NULL, "mwait_is_nop", "", "", 0));
  // Unknown MSRs raise #GP, as in QEMU.
  sim->add(BXPN_IGNORE_BAD_MSRS, new bx_param_bool_c(NULL, "ignore_bad_msrs", "", "", 0));
  sim->add(BXPN_FORCE_IGNNE, new bx_param_bool_c(NULL, "force_ignne", "", "", 0));
  sim->add(BXPN_PORT_E9_HACK, new bx_param_bool_c(NULL, "enabled", "", "", 0));
  sim->add(BXPN_PORT_E9_HACK_ALL_RINGS, new bx_param_bool_c(NULL, "all_rings", "", "", 0));
  sim->add(BXPN_IODEBUG_ALL_RINGS, new bx_param_bool_c(NULL, "all_rings", "", "", 0));
  sim->add(BXPN_CPU_ADD_FEATURES, new bx_param_string_c(NULL, "add_features", "", "", ""));
  sim->add(BXPN_CPU_EXCLUDE_FEATURES, new bx_param_string_c(NULL, "exclude_features", "", "", ""));
  sim->add(BXPN_CONFIGURABLE_MSRS_PATH, new bx_param_string_c(NULL, "msrs", "", "", ""));
  sim->add(BXPN_BRAND_STRING, new bx_param_string_c(NULL, "brand_string", "", "", ""));
}

// ---------------------------------------------------------------------------
// Devices: port I/O and the 8259 go to the host; the other device hooks the
// CPU core has are not wired on this board.

class softcpu_pic_c : public bx_pic_stub_c {
public:
  virtual Bit8u IAC(void) { return host.iac(host.ctx); }
};

static softcpu_pic_c softcpu_pic;

// Only read for a log message (the shutdown status byte on a triple fault).
class softcpu_cmos_c : public bx_cmos_stub_c {
public:
  virtual Bit32u get_reg(Bit8u reg) { return 0; }
};

static softcpu_cmos_c softcpu_cmos;

// A hardware reset from inside the CPU core (a triple fault): the whole
// machine resets, which is the host's to do.
static bool reset_requested;

bx_devices_c bx_devices;

bx_devices_c::bx_devices_c()
{
  put("devices", "DEV");
  init_stubs();
  pluginPicDevice = &softcpu_pic;
  pluginCmosDevice = &softcpu_cmos;
  bulkIOHostAddr = NULL;
  bulkIOQuantumsRequested = 0;
  bulkIOQuantumsTransferred = 0;
}

bx_devices_c::~bx_devices_c() {}

void bx_devices_c::init_stubs()
{
  pluginCmosDevice = &stubCmos;
  pluginDmaDevice = &stubDma;
  pluginHardDrive = &stubHardDrive;
  pluginPicDevice = &stubPic;
  pluginPitDevice = &stubPit;
  pluginSpeaker = &stubSpeaker;
  pluginVgaDevice = &stubVga;
#if BX_SUPPORT_APIC
  pluginIOAPIC = &stubIOAPIC;
#endif
  pluginExtFpuIRQ = &stubExtFpuIRQ;
}

// The devices are the host's: on a hardware reset after power-on, stop and
// let the host reset the machine (see softcpu_reset_requested).
static bool powering_on;

void bx_devices_c::reset(unsigned type)
{
  if (!powering_on) {
    reset_requested = true;
    stop_all();
  }
}
void bx_devices_c::exit(void) {}

Bit32u bx_devices_c::inp(Bit16u addr, unsigned io_len)
{
  return host.io_read(host.ctx, addr, io_len);
}

void bx_devices_c::outp(Bit16u addr, Bit32u value, unsigned io_len)
{
  host.io_write(host.ctx, addr, io_len, value);
}

// ---------------------------------------------------------------------------
// Memory: RAM at 0, the BIOS at the top of 4 GiB, read-only; everything
// else (the VGA window 0xA0000-0xBFFFF, device registers) is the host's.
// The local APIC never gets here: the CPU core handles it.

static bool is_ram(bx_phy_address a)
{
  return a < ram_size && !(a >= 0xA0000 && a < 0xC0000);
}

static bool is_bios(bx_phy_address a)
{
  return a >= bios_base && a < BX_CONST64(0x100000000);
}

// Device accesses in pieces of at most 8 bytes, as the board takes them.
static void mmio(bx_phy_address addr, unsigned len, Bit8u *data, bool write)
{
  while (len) {
    unsigned n = 1;
    while (n < 8 && n * 2 <= len && (addr & (n * 2 - 1)) == 0) n *= 2;
    if (write) {
      Bit64u v = 0;
      memcpy(&v, data, n);
      host.mmio_write(host.ctx, addr, n, v);
    } else {
      Bit64u v = host.mmio_read(host.ctx, addr, n);
      memcpy(data, &v, n);
    }
    addr += n;
    data += n;
    len -= n;
  }
}

Bit8u *BX_MEM_C::getHostMemAddr(BX_CPU_C *cpu, bx_phy_address addr, unsigned rw)
{
  bx_phy_address a = A20ADDR(addr);
  if (is_ram(a)) return ram + a;
  if (is_bios(a) && !(rw & 1)) return (Bit8u *) bios + (a - bios_base);
  return NULL;
}

void BX_MEM_C::readPhysicalPage(BX_CPU_C *cpu, bx_phy_address addr, unsigned len, void *data)
{
  bx_phy_address a = A20ADDR(addr);
  if (is_ram(a)) {
    memcpy(data, ram + a, len);
  } else if (is_bios(a)) {
    memcpy(data, bios + (a - bios_base), len);
  } else {
    mmio(a, len, (Bit8u *) data, false);
  }
}

void BX_MEM_C::writePhysicalPage(BX_CPU_C *cpu, bx_phy_address addr, unsigned len, void *data)
{
  bx_phy_address a = A20ADDR(addr);
  if (is_ram(a)) {
    pageWriteStampTable.decWriteStamp(a, len);
    memcpy(ram + a, data, len);
  } else if (is_bios(a)) {
    // Flash: writes are ignored.
  } else {
    mmio(a, len, (Bit8u *) data, true);
  }
}

bool BX_MEM_C::dbg_fetch_mem(BX_CPU_C *cpu, bx_phy_address addr, unsigned len, Bit8u *buf)
{
  readPhysicalPage(cpu, addr, len, buf);
  return true;
}

Bit64u BX_MEMORY_STUB_C::get_memory_len(void) { return ram_size; }

// ---------------------------------------------------------------------------
// The run loop and the C interface.

static int deadline_timer = -1;
static int slice_timer = -1;

static void stop_all(void)
{
  bx_pc_system.kill_bochs_request = 1;
  for (unsigned i = 0; bx_cpu_array && i < BX_SMP_PROCESSORS; i++) {
    if (BX_CPU(i)) BX_CPU(i)->async_event |= 1;
  }
}

static void deadline_handler(void *)
{
  if (host.deadline) host.deadline(host.ctx);
}

static void slice_handler(void *)
{
  stop_all();
}

static void delete_cpus(void)
{
  if (!bx_cpu_array) return;
  for (unsigned i = 0; i < BX_SMP_PROCESSORS; i++) delete BX_CPU(i);
  delete [] bx_cpu_array;
  bx_cpu_array = NULL;
}

extern "C" int softcpu_init(const softcpu_host *h, uint32_t ncpus, uint8_t *ram_ptr, uint64_t ram_len,
                            const uint8_t *bios_ptr, uint32_t bios_len)
{
  error_text.clear();
  if (((uintptr_t) ram_ptr | (uintptr_t) bios_ptr) & 0xfff) {
    error_text = "[softcpu] RAM and BIOS must be 4 KiB aligned";
    return -1;
  }
  if (ncpus < 1 || ncpus > 64) {
    error_text = "[softcpu] 1 to 64 processors";
    return -1;
  }
  host = *h;
  host_set = true;
  pluginlog = genlog = siminterface_log = &softcpu_log;
  softcpu_log.put("softcpu", "CPU");
  init_params();

  delete_cpus();
  ram = ram_ptr;
  ram_size = ram_len;
  bios = bios_ptr;
  bios_base = BX_CONST64(0x100000000) - bios_len;

  sim->num(BXPN_CPU_NPROCESSORS)->set(ncpus);
  bx_cpu_count = ncpus;
  simulate_xapic = true;
  apic_id_mask = 0xFF;

  static bool timers;
  if (!timers) {
    bx_pc_system.initialize((Bit32u) sim->num(BXPN_IPS)->get());
    deadline_timer = bx_pc_system.register_timer_ticks(NULL, deadline_handler, 1, 0, 0, "host");
    slice_timer = bx_pc_system.register_timer_ticks(NULL, slice_handler, 1, 0, 0, "slice");
    timers = true;
  } else {
    bx_pc_system.deactivate_timer(deadline_timer);
    bx_pc_system.deactivate_timer(slice_timer);
  }

  bx_cpu_array = new BX_CPU_C *[ncpus];
  for (unsigned i = 0; i < ncpus; i++) {
    BX_CPU(i) = new BX_CPU_C(i);
    BX_CPU(i)->initialize();  // assigns the local APIC ID
    BX_CPU(i)->sanity_checks();
  }
  powering_on = true;
  bx_pc_system.Reset(BX_RESET_HARDWARE);
  powering_on = false;
  reset_requested = false;
  // As QEMU: after reset EDX holds the processor signature (CPUID.1:EAX).
  for (unsigned i = 0; i < ncpus; i++) {
    uint32_t r[4] = {0, 0, 0, 0};
    host.cpuid(host.ctx, i, 1, 0, r);
    BX_CPU(i)->gen_reg[BX_32BIT_REG_EDX].rrx = r[0];
  }
  bx_pc_system.kill_bochs_request = 0;
  if (!error_text.empty()) return -1;
  return 0;
}

extern "C" uint64_t softcpu_ticks(void)
{
  return bx_pc_system.time_ticks();
}

extern "C" void softcpu_set_deadline(uint64_t tick)
{
  Bit64u now = bx_pc_system.time_ticks();
  bx_pc_system.activate_timer_ticks(deadline_timer, tick > now ? tick - now : 1, 0);
}

extern "C" void softcpu_set_intr(int level)
{
  if (level) BX_CPU(0)->raise_INTR();
  else BX_CPU(0)->clear_INTR();
}

extern "C" void softcpu_mem_written(uint64_t addr, uint64_t len)
{
  for (Bit64u page = addr & ~BX_CONST64(0xfff); page < addr + len; page += 0x1000)
    pageWriteStampTable.decWriteStamp(page);
}

extern "C" void softcpu_advance(uint64_t ticks)
{
  // tickn takes 32-bit steps.
  while (ticks) {
    Bit32u n = ticks > 0x7fffffff ? 0x7fffffff : (Bit32u) ticks;
    bx_pc_system.tickn(n);
    ticks -= n;
  }
}

extern "C" void softcpu_stop(void)
{
  stop_all();
}

extern "C" int softcpu_all_idle(void)
{
  for (unsigned i = 0; i < BX_SMP_PROCESSORS; i++) {
    if (BX_CPU(i)->activity_state == BX_CPU_C::BX_ACTIVITY_STATE_ACTIVE) return 0;
  }
  return 1;
}

extern "C" int softcpu_long_mode(uint32_t cpu)
{
  return cpu < BX_SMP_PROCESSORS && BX_CPU(cpu)->long_mode();
}

extern "C" int softcpu_waiting_for_sipi(uint32_t cpu)
{
  return cpu < BX_SMP_PROCESSORS && BX_CPU(cpu)->activity_state == BX_CPU_C::BX_ACTIVITY_STATE_WAIT_FOR_SIPI;
}

extern "C" uint64_t softcpu_rip(uint32_t cpu)
{
  return cpu < BX_SMP_PROCESSORS ? BX_CPU(cpu)->get_instruction_pointer() : 0;
}

extern "C" int softcpu_reset_requested(void)
{
  return reset_requested;
}

extern "C" const char *softcpu_error(void)
{
  return error_text.c_str();
}

extern "C" int softcpu_all_idle(void);

// Set by PAUSE (patched, see build.rs) on a machine with several processors.
bool softcpu_yield;

// Instructions a processor may run in one turn when it doesn't yield.
static const Bit64u TURN = 1000;

// As bx_begin_simulation in Bochs's main.cc, with one change: one processor
// runs until it is asked to stop; several take turns, and the tick count
// advances by the instructions each one ran. A turn lasts until the
// processor executes PAUSE or HLT or has run TURN instructions, as in
// QEMU's round-robin loop (Bochs's own loop switches after every trace,
// which can starve a processor spinning on a lock, see build.rs).
extern "C" int softcpu_run(uint64_t ticks)
{
  if (!error_text.empty()) return -1;
  bx_pc_system.kill_bochs_request = 0;
  bx_pc_system.activate_timer_ticks(slice_timer, ticks ? ticks : 1, 0);

  if (BX_SMP_PROCESSORS == 1) {
    while (!bx_pc_system.kill_bochs_request) BX_CPU(0)->cpu_loop();
  } else {
    static Bit32u executed, processor;
    bool run = true;
    if (setjmp(BX_CPU_C::jmp_buf_env)) {
      // can get here only from exception function or VMEXIT
      BX_CPU(processor)->icount++;
      run = false;
    }
    while (!bx_pc_system.kill_bochs_request) {
      if (run) {
        BX_CPU_C *cpu = BX_CPU(processor);
        Bit64u start = cpu->get_icount();
        softcpu_yield = false;
        for (;;) {
          Bit64u before = cpu->get_icount();
          cpu->cpu_run_trace();
          Bit64u now = cpu->get_icount();
          if (now == before || softcpu_yield || now - start >= TURN || bx_pc_system.kill_bochs_request) break;
        }
      }
      else run = true;
      Bit32u n = (Bit32u)(BX_CPU(processor)->get_icount() - BX_CPU(processor)->icount_last_sync);
      if (n == 0) n = sim->num(BXPN_SMP_QUANTUM)->get();  // the CPU was halted
      executed += n;
      if (++processor == BX_SMP_PROCESSORS) {
        processor = 0;
        BX_TICKN(executed / BX_SMP_PROCESSORS);
        executed %= BX_SMP_PROCESSORS;
        // All halted: let time pass to the next timer at once, as a single
        // processor's HLT does (handleWaitForEvent), instead of in quanta.
        if (softcpu_all_idle() && !bx_pc_system.kill_bochs_request) {
          Bit32u left = bx_pc_system.getNumCpuTicksLeftNextEvent();
          if (left > 1) BX_TICKN(left - 1);
        }
      }
      BX_CPU(processor)->icount_last_sync = BX_CPU(processor)->get_icount();
    }
  }

  bx_pc_system.deactivate_timer(slice_timer);
  // async_event stays set: it only makes each CPU look at its events again.
  bx_pc_system.kill_bochs_request = 0;
  return error_text.empty() ? 0 : -1;
}
