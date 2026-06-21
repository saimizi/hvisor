# hvisor Overview

> A learner-oriented overview of **hvisor**, a Type-1 (bare-metal) hypervisor
> written in Rust. This document explains *what* hvisor is, *how* it is
> structured, and *how* it boots and runs guests, with pointers into the
> source tree so you can dig deeper.

---

## 1. What is hvisor?

hvisor is a **Type-1 bare-metal Virtual Machine Monitor (VMM)** developed by the
[syswonder](https://www.syswonder.org) group. "Type-1" means it runs **directly on
the hardware** — there is no host operating system beneath it (contrast with Type-2
hypervisors like VirtualBox/QEMU-KVM-as-app, which run as a process on a host OS).

hvisor's defining idea is a **separation kernel** design: instead of time-sharing
hardware among many VMs with a complex scheduler (like KVM or Xen), hvisor
**statically partitions** the physical machine into isolated compartments and then
mostly gets out of the way. This makes it small, predictable, and well-suited to
mixed-criticality and real-time use cases.

Key properties:

- **Multi-architecture:** `aarch64`, `riscv64`, `loongarch64`, `x86_64`.
- **Written in Rust**, `#![no_std]`, no host OS.
- **Static resource partitioning** — CPUs, memory, and devices are assigned at
  configuration time, not scheduled dynamically.
- **Managed from Linux** running in the privileged "root zone" via the companion
  userspace tool [hvisor-tool](https://github.com/syswonder/hvisor-tool).

Some of its design borrows ideas from [jailhouse](https://github.com/siemens/jailhouse)
(static partitioning) and [RVM1.5](https://github.com/rcore-os/RVM1.5) (Rust VMM).

---

## 2. The core concept: Zones

A **zone** is hvisor's unit of isolation — essentially a VM/partition. The codebase
models it as `struct Zone` in [`src/zone.rs`](../src/zone.rs).

There are three conceptual kinds of zone:

| Zone      | Role                                                                 |
|-----------|---------------------------------------------------------------------|
| **zone0** | Management zone ("root zone"), runs `root-linux`. Boots first, owns management hypercalls, creates/starts/stops other zones. |
| **zoneU** | User zones — ordinary guests (Linux, Android, etc.).                |
| **zoneR** | Real-time zones — guests with real-time OSes (e.g. RT-Thread, Zephyr). |

Each zone owns a fixed slice of the machine. Looking at `ZoneInner` in
`src/zone.rs`, a zone holds:

- `cpu_set` / `cpu_num` — which physical CPUs belong to it (a static bitmap).
- `gpm: MemorySet<Stage2PageTable>` — its **guest physical memory**, expressed as a
  second-stage page table (guest-physical → host-physical).
- `mmio` — registered MMIO regions and their emulation handlers.
- `irq_bitmap` — which interrupts this zone is allowed to receive.
- `vpci_bus` — a virtual PCIe root complex for the zone.
- `iommu_pt` — optional IOMMU page table for DMA isolation.

The global list of zones lives in `ZONE_LIST`; `root_zone()` returns zone0.

---

## 3. How hvisor virtualizes the machine

The "Type-1 hypervisor" concepts you are learning map onto hvisor like this:

### CPU virtualization — static partitioning, no scheduler
Each physical CPU (pCPU) is bound to exactly one zone and runs that zone's vCPU.
There is **no time-sharing and no scheduler**. A pCPU enters the guest and only
returns to the hypervisor on a *trap* (an exception/VM-exit caused by a privileged
operation, an interrupt, or a hypercall). This is what keeps hvisor simple and
deterministic. Per-CPU state lives in `PerCpu` ([`src/cpu_data.rs`](../src/cpu_data.rs)),
including the architecture-specific `arch_cpu` that actually enters guest mode.

### Memory virtualization — second-stage page tables
Guest memory is **pre-allocated via configuration** and mapped through the CPU's
**second-stage translation** (ARM stage-2, RISC-V G-stage, Intel EPT, LoongArch
equivalent). The guest sees contiguous "guest-physical" memory; the hardware
translates it to real host-physical addresses using the zone's `gpm`. Hypervisor
memory management lives in [`src/memory/`](../src/memory/) (`frame.rs` frame
allocator, `mapper.rs`/`mm.rs` page tables, `mmio.rs` MMIO dispatch).

### I/O virtualization — passthrough + virtio
Two models:
- **Passthrough:** a device is assigned directly to a zone (with IOMMU protection
  where available). Fast, simple, but the device belongs to one zone only.
- **virtio paravirtualization:** the backend runs in zone0/userspace (hvisor-tool),
  and guests talk to it via virtio-mmio/virtio-pci. Used for `virtio-blk`,
  `virtio-net`, `virtio-console`, `virtio-gpu`.

### Interrupt virtualization
Per-architecture interrupt controller virtualization lives in
[`src/device/irqchip/`](../src/device/irqchip/): GIC (aarch64), PLIC/AIA (riscv64),
APIC (x86_64), 7A2000 (loongarch64). The zone's `irq_bitmap` decides which IRQs it
may receive.

---

## 4. Source tree map

```
src/
├── main.rs        Entry point and multi-core boot/init sequencing
├── zone.rs        The Zone abstraction (a VM/partition) + zone lifecycle
├── cpu_data.rs    PerCpu state, vCPU state machine, run_vm()
├── config.rs      Parsing of zone configuration (CPUs/memory/devices)
├── consts.rs      Memory layout constants, MAX_CPU_NUM, etc.
├── event.rs       Inter-processor events / IPIs between pCPUs
├── error.rs       HvResult / error types and macros
├── logging.rs     Logging setup
├── panic.rs       Panic handler (no_std)
│
├── arch/          Architecture-specific virtualization (the "hardware magic")
│   ├── aarch64/   EL2 setup, GIC, stage-2, trap handling
│   ├── riscv64/   H-extension, PLIC/AIA, G-stage
│   ├── loongarch64/
│   └── x86_64/    VT-x (VMX), EPT, APIC
│
├── memory/        Frame allocator, heap, page-table mapper, MMIO dispatch
├── hypercall/     Guest → hypervisor call interface (mod.rs)
├── device/        irqchip/, uart/, iommu/, sifive_ccache/
└── pci/           PCIe virtualization (virtual root complex, config accessors)
```

Platform/board specifics live under [`platform/`](../platform/) and are selected at
build time (see `Makefile`, `build.rs`, and Cargo features).

---

## 5. Boot and initialization flow

The boot sequence is driven by `rust_main()` in [`src/main.rs`](../src/main.rs). hvisor
brings up all CPUs and uses simple atomic counters as barriers so the cores advance
together. Simplified flow:

```
                 ┌─────────────────────────── primary (master) CPU ───────────────────────────┐
arch_entry ─► rust_main(cpuid, dtb)
   │  install_trap_vector()
   │  (first CPU wins MASTER_CPU) ─► percpu::init, heap::init, timebase, post-heap init
   │  PerCpu::new(cpuid)
   │  wakeup_secondary_cpus()  ──────────────►  each secondary: arch_entry ─► rust_main(...)
   │  barrier: wait until ENTERED_CPUS == MAX_CPU_NUM   (all cores checked in)
   │  arch_setup_parange()
   │  primary_init_early():                          secondaries: wait_for(INIT_EARLY_OK)
   │      logging, frame::init                            │
   │      arch::stage2_mode_detect()                      │
   │      irqchip::primary_init_early()                   │
   │      (iommu_init, pci init if enabled)               │
   │      root_zone_config() ─► zone_create() ─► add_zone()   ◄── zone0 is built here
   │      INIT_EARLY_OK = 1  ─────────────────────────────┘
   │  per_cpu_init(); irqchip::percpu_init()
   │  barrier: wait until INITED_CPUS == MAX_CPU_NUM
   │  primary_init_late():                            secondaries: wait_for(INIT_LATE_OK)
   │      irqchip::primary_init_late()
   │      INIT_LATE_OK = 1
   │
   └─► cpu.run_vm()   ◄── every CPU ends here, entering its guest
```

Notable detail in `run_vm()` ([`src/cpu_data.rs`](../src/cpu_data.rs)): only the
zone's **boot CPU** starts the guest immediately; the other CPUs assigned to that
zone first `idle()` and are later released (e.g. via PSCI/SGI/IPI) when the guest
brings its secondary cores online.

### What `zone_create()` does (zone0 and later guests)
From `zone_create()` in [`src/zone.rs`](../src/zone.rs):

1. Allocate a `Zone` with a fresh stage-2 `MemorySet` (`gpm`).
2. `pt_init()` — map the configured memory regions into stage-2.
3. `mmio_init()` — register MMIO emulation handlers (interrupt controller, etc.).
4. Bind the configured CPUs into the zone's `cpu_set` (erroring if a CPU is already
   taken by another zone — that's the static-partitioning guarantee).
5. Initialize virtual PCI / IOMMU if enabled.
6. Arch-specific pre/post configuration and reset hooks.
7. Initialize the virtual interrupt controller (`virqc_init`) and `irq_bitmap`.
8. Compute the DTB guest-physical address and record per-CPU entry point.

Later guest zones (zoneU/zoneR) are created the same way, but at **runtime**,
triggered from zone0 via the `HvZoneStart` hypercall.

---

## 6. The hypercall interface (guest → hypervisor)

Guests and zone0 communicate with hvisor through **hypercalls** — a trap into the
hypervisor with a numeric code and arguments. They are dispatched by
`HyperCall::hypercall()` in [`src/hypercall/mod.rs`](../src/hypercall/mod.rs).
The defined codes (`HyperCallCode`) are:

| Code | Name                  | Purpose                                            |
|-----:|-----------------------|----------------------------------------------------|
| 0    | `HvVirtioInit`        | Initialize the virtio shared region/backend.       |
| 1    | `HvVirtioInjectIrq`   | Inject a virtio interrupt into a guest.            |
| 86   | `HvVirtioGetIrq`      | Query pending virtio IRQ.                          |
| 2    | `HvZoneStart`         | Create & start a new guest zone (from zone0).      |
| 3    | `HvZoneShutdown`      | Stop/destroy a zone.                              |
| 4    | `HvZoneList`          | Enumerate zones (used by hvisor-tool).            |
| 20   | `HvClearInjectIrq`    | Clear an injected IRQ.                            |
| 5    | `HvIvcInfo`           | Inter-VM communication info.                       |
| 6    | `HvConfigCheck`       | Validate/inspect configuration.                    |
| 7    | `HvVirtioPCI`         | virtio-pci related operation.                     |

Most management hypercalls are privileged and only meaningful from **zone0**, which
is how a Linux userspace tool (hvisor-tool) can create/start/stop guests.

---

## 7. Mental model summary

Think of hvisor as a **static hardware partitioner**, not a time-sharing scheduler:

1. At boot, the primary CPU builds **zone0** (root Linux) and hands every CPU its
   guest. CPUs run guests directly; the hypervisor is dormant until a **trap**.
2. zone0's Linux, via **hypercalls** issued by **hvisor-tool**, asks hvisor to
   **create and start** further zones (zoneU/zoneR), each carved from spare CPUs,
   pre-reserved memory, and assigned devices.
3. Isolation is enforced by hardware: **stage-2 page tables** for memory, the
   **virtual interrupt controller + irq_bitmap** for interrupts, and the **IOMMU**
   for DMA.
4. Because partitioning is static, there is no inter-zone scheduling — which is
   exactly what gives hvisor its small size and real-time predictability.

---

## 8. Where to go next (suggested reading order)

1. [`src/main.rs`](../src/main.rs) — boot/init barrier sequencing (Section 5).
2. [`src/zone.rs`](../src/zone.rs) + [`src/config.rs`](../src/config.rs) — what a VM *is*.
3. [`src/arch/<your-arch>/`](../src/arch/) — the actual CPU virtualization and trap
   handling. (aarch64 is the most complete reference; riscv64 is conceptually the
   simplest; x86_64/VMX is the most intricate.)
4. [`src/memory/`](../src/memory/) — second-stage address translation.
5. [`src/device/irqchip/`](../src/device/irqchip/) — interrupt virtualization.
6. [`src/hypercall/mod.rs`](../src/hypercall/mod.rs) — the management interface.

For build/run instructions on real boards and QEMU, see the official docs:
<https://hvisor.syswonder.org/>.
