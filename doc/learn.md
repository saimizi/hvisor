# How to Read the hvisor Source Code

A guided reading path for someone learning hvisor. The key idea: read
**top-down, following the boot flow**, because hvisor is small and almost
everything branches off `rust_main()`. Resist diving into a function's body
until you have seen *who calls it*.

> See also [`overview.md`](overview.md) (the big-picture map) and
> [`irq_overview.md`](irq_overview.md) (interrupt handling in depth).

---

## Step 0 — Read the docs first

Start with [`overview.md`](overview.md). It gives you the one idea that makes
the rest of the code make sense:

> **hvisor is a static partitioner, not a scheduler.**

Once that clicks, the absence of a scheduler / run-queue stops being confusing.
Read [`irq_overview.md`](irq_overview.md) later, when you reach the device layer
(Step 5).

---

## Step 1 — `src/main.rs` (the spine)

The single most important file. Read `rust_main()` end to end and trace:

- The **multi-core barrier dance** — `MASTER_CPU`, `ENTERED_CPUS`,
  `INITED_CPUS`, and the `wait_for_counter()` barriers. Every CPU runs the
  *same* function; the atomics are what keep them in lockstep.
- `primary_init_early()` — where **zone0 (root Linux) is built** via
  `zone_create()`.
- The final line, `cpu.run_vm()` — every CPU ends here, entering its guest.

Don't chase function bodies yet. Just get the *shape* of boot. Section 5 of
[`overview.md`](overview.md) is the diagram for exactly this.

---

## Step 2 — `src/zone.rs` + `src/config.rs` (what a "VM" is)

`zone_create()` is the heart of hvisor. Read it slowly — it builds the stage-2
page tables, registers MMIO handlers, claims CPUs, and sets up the virtual
interrupt controller. `config.rs` tells you *what* gets fed in (CPUs / memory /
devices per zone). The "What `zone_create()` does" subsection of
[`overview.md`](overview.md) is your checklist here.

---

## Step 3 — `src/cpu_data.rs` (the vCPU loop)

Read `PerCpu` and `run_vm()`. This is the trap-and-resume loop:

```
enter guest → hardware traps back on a privileged event → handle → resume
```

Note the detail that only a zone's **boot CPU** starts immediately; the other
CPUs assigned to that zone idle until released (e.g. via PSCI / SGI / IPI).

---

## Step 4 — Pick ONE architecture: `src/arch/<arch>/`

Don't read all four arch backends. Pick one and go deep:

- **aarch64** — most complete reference (suggested default).
- **riscv64** — conceptually the simplest.
- **x86_64** — VMX, the most intricate.
- **loongarch64**

Look for: privileged-mode (e.g. EL2) setup, the trap vector / exit-reason
dispatch, and stage-2 translation. This is where the "hardware magic" lives.

---

## Step 5 — `src/memory/` then `src/device/irqchip/`

- `src/memory/` — how isolation is *enforced*: stage-2 page tables, the frame
  allocator, and MMIO dispatch.
- `src/device/irqchip/` — interrupt virtualization. Read
  [`irq_overview.md`](irq_overview.md) in full **now**, because it explains the
  "route physically, inject virtually" model that this code implements.

---

## Step 6 — `src/hypercall/mod.rs` (the control plane)

Read this last, because it only makes sense once you know what a zone is. This
is how zone0's Linux (via hvisor-tool) asks hvisor to create / start / stop
other guests. See Section 6 of [`overview.md`](overview.md) for the hypercall
table.

---

## The one tip that matters most

Read in the order things **boot**, and resist diving into a function's body
until you have seen who calls it. hvisor is layered exactly like its boot
sequence:

```
main.rs ─► zone.rs ─► cpu_data.rs ─► arch/<arch>/ ─► memory/ + irqchip/ ─► hypercall/
 (spine)   (a VM)     (vCPU loop)    (HW magic)      (isolation + IRQs)    (control plane)
```
