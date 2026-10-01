/* The software CPU's C interface: Bochs's x86-64 CPU core, with memory,
 * port I/O, the 8259's interrupt line and CPUID answered by the host
 * (TempleOS.exe's own board, see softcpu/src/lib.rs).
 *
 * Bochs keeps its CPUs, timers and memory in globals, so there is one
 * machine per process; softcpu_init replaces the previous one.
 *
 * Time is counted in ticks: one tick is one instruction on each CPU (as in
 * Bochs's SMP loop, and QEMU's -icount shift=0).
 */
#ifndef SOFTCPU_H
#define SOFTCPU_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct softcpu_host {
    void *ctx;
    /* Port I/O, len 1, 2 or 4. */
    uint32_t (*io_read)(void *ctx, uint16_t port, uint32_t len);
    void (*io_write)(void *ctx, uint16_t port, uint32_t len, uint32_t value);
    /* Physical accesses outside RAM and the BIOS: the VGA window and
     * device registers. len 1, 2, 4 or 8, naturally aligned. */
    uint64_t (*mmio_read)(void *ctx, uint64_t addr, uint32_t len);
    void (*mmio_write)(void *ctx, uint64_t addr, uint32_t len, uint64_t value);
    /* 8259 interrupt acknowledge: the vector of the interrupt the BSP takes. */
    uint8_t (*iac)(void *ctx);
    /* CPUID on processor `cpu`: out = eax, ebx, ecx, edx. */
    void (*cpuid)(void *ctx, uint32_t cpu, uint32_t leaf, uint32_t subleaf, uint32_t out[4]);
    /* The deadline set with softcpu_set_deadline has come. */
    void (*deadline)(void *ctx);
    /* Bochs's messages: level 0 debug, 1 info, 2 error, 3 panic. */
    void (*log)(void *ctx, int level, const char *msg);
} softcpu_host;

/* A new machine: `ncpus` processors (APIC IDs 0..ncpus-1) after a hardware
 * reset, `ram` (host memory, `ram_size` bytes) at guest physical 0, and the
 * BIOS (`bios_size` bytes, a multiple of 64 KiB) at the top of 4 GiB. Both
 * must be 4 KiB aligned: Bochs's TLB ORs page offsets onto host addresses.
 * 0xA0000-0xBFFFF and everything else unmapped go to mmio_read/mmio_write.
 * Returns 0, or -1 with softcpu_error() set. */
int softcpu_init(const softcpu_host *host, uint32_t ncpus, uint8_t *ram, uint64_t ram_size,
                 const uint8_t *bios, uint32_t bios_size);

/* Run until `ticks` more ticks have passed, softcpu_stop is called (from a
 * host callback), or Bochs reports a fatal error. Returns 0, or -1 with
 * softcpu_error() set. */
int softcpu_run(uint64_t ticks);

/* Ticks since softcpu_init. */
uint64_t softcpu_ticks(void);

/* Call `deadline` when the tick count reaches `tick` (replaces the
 * previous deadline; a tick already passed fires at once). */
void softcpu_set_deadline(uint64_t tick);

/* The 8259's INTR output, wired to the BSP (LINT0, virtual wire). */
void softcpu_set_intr(int level);

/* The host wrote guest RAM directly (e.g. DMA): forget any instructions
 * decoded from those bytes. */
void softcpu_mem_written(uint64_t addr, uint64_t len);

/* Let `ticks` ticks pass without running instructions (timers fire as
 * they come due). Only between softcpu_run calls. */
void softcpu_advance(uint64_t ticks);

/* Return from softcpu_run as soon as possible. */
void softcpu_stop(void);

/* True when every processor is halted or waiting for a SIPI. */
int softcpu_all_idle(void);

/* Processor `cpu` is in long mode (EFER.LMA). */
int softcpu_long_mode(uint32_t cpu);

/* Processor `cpu` waits for a SIPI (as application processors do from
 * reset until started). */
int softcpu_waiting_for_sipi(uint32_t cpu);

/* RIP of processor `cpu`, for error reports. */
uint64_t softcpu_rip(uint32_t cpu);

/* The machine was reset from inside the core (a triple fault, as QEMU's
 * -no-reboot-less default): softcpu_run returned for the host to reset the
 * whole machine. */
int softcpu_reset_requested(void);

const char *softcpu_error(void);

#ifdef __cplusplus
}
#endif

#endif
