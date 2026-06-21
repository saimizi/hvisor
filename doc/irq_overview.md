# hvisor Interrupt (IRQ) Handling Overview

> A learner-oriented walkthrough of how hvisor virtualizes hardware interrupts.
> The concrete code examples use the **aarch64 / GICv3** path, but the same
> principles apply to the other interrupt controllers hvisor supports: PLIC/AIA
> (riscv64), APIC (x86_64), and ls7a2000 (loongarch64).
>
> See also [`overview.md`](./overview.md) for the big picture.

---

## 1. Guiding principle: interrupts are statically partitioned

Just like CPUs, memory, and devices, **interrupts are partitioned per zone — not
funneled to zone0.** Each zone owns a fixed set of IRQs, and the interrupt
controller is programmed to deliver a device's interrupt directly to the physical
CPU running the **owning** zone.

- Each zone has an `irq_bitmap` (`src/zone.rs:257`, `irq_in_zone()`), initialized
  at zone creation from the zone configuration (`zone.rs:908`).
- A device assigned to a guest zone raises its interrupt straight to that guest's
  pCPU. zone0 is *not* in the path — it only receives interrupts for devices
  assigned to zone0.

This is what keeps interrupt latency low and predictable (no detour through a
controller VM), a key reason hvisor suits real-time / mixed-criticality systems.

---

## 2. The two-part mechanism: route physically, inject virtually

### Part 1 — Physical routing
Based on each zone's `irq_bitmap`, the GIC **distributor** is configured so the
physical interrupt for a device is delivered to the physical CPU that runs the
owning zone.

### Part 2 — Virtual injection
hvisor still configures the CPU so that physical IRQs trap to **EL2** (the
hypervisor) via `HCR_EL2.IMO`. The hypervisor acknowledges the physical interrupt
and then **injects a virtual interrupt** into the guest currently running on that
CPU, using the GIC's hardware virtualization (List Registers). The guest then
handles the virtual interrupt as if it were a normal device interrupt.

---

## 3. The full delivery path (what actually happens on a HW IRQ)

When a device interrupt fires while a guest (EL1) is executing on its CPU:

```
guest (EL1) running an application
      │  device IRQ → HCR_EL2.IMO routes physical IRQ to EL2
      ▼
EL2 exception vector 0x480 (EXIT_REASON_EL1_IRQ)        ── this is the "VMEXIT"
      │  handle_vmexit (trap.S): save guest x0..x30 onto the per-CPU EL2 stack
      ▼
arch_handle_exit (trap.rs:108) → irqchip_handle_irq1() → gic_handle_irq()
      │  [runs at EL2, on the per-CPU hypervisor stack, same physical CPU]
      │  - pending_irq(): read icc_iar1_el1 (acknowledge the physical IRQ)
      │  - inject_irq(irq_id, is_hardware=true): write it into a List Register
      │  - deactivate_irq()
      ▼
vmreturn (trap.rs:497) → ERET                            ── this is the "VMRESUME"
      ▼
guest (EL1) resumes and now takes the *virtual* interrupt → runs its ISR once
```

### The context `gic_handle_irq()` runs in
- **Exception level:** EL2 (hypervisor). The guest has been "exited."
- **CPU:** the *same* physical CPU that was running the guest (the IRQ was routed
  there). No cross-CPU hop.
- **Stack:** the per-CPU hypervisor stack, *not* the guest stack.
- **Guest state:** saved as a `GeneralRegisters` struct on that stack (the
  `handle_vmexit` macro in `trap.S` pushes `x0..x30`); the guest is frozen at the
  instruction boundary where the IRQ hit.

### ARM "VMEXIT" vs x86
On x86 (VT-x) this is a literal `VMEXIT` driven by the VMCS, with hardware saving a
defined guest-state area. On ARM/RISC-V/LoongArch there is **no** VMCS-style atomic
world switch — it is "just" an exception to a higher privilege level (EL2 / RISC-V
HS-mode), and hvisor **manually** saves/restores registers (`handle_vmexit` /
`vmreturn`). The *concept* — trap out, handle, resume — is identical, which is why
the code uses the "vmexit" name (see the comment at `trap.rs:108`).

### Note on `EXIT_REASON_EL2_IRQ`
An IRQ taken *while already in EL2* lands in `irqchip_handle_irq2()` (`trap.rs:132`),
which currently just errors and spins. hvisor does not expect to take device
interrupts while executing hypervisor code; it handles them only on the
guest → EL2 exit path.

---

## 4. "Interrupted twice" — what that really means

For an application running inside a guest, the physical CPU it runs on is diverted
**twice** by one hardware IRQ — but at **different privilege levels**, and only one
of them runs guest code:

| Step | Exception level | Who runs code | Guest ISR runs? |
|------|-----------------|---------------|-----------------|
| 1. Physical IRQ → "VMEXIT" | **EL2** (hypervisor) | `gic_handle_irq()` | **No** — guest is frozen |
| 2. Virtual IRQ after `ERET` | **EL1** (guest) | guest's interrupt handler | **Yes — once** |

So:
- At the level of **CPU exception entries on that core** → two (one EL2, one EL1).
- In terms of the **guest servicing the interrupt** → exactly **once** (step 2).
  The VMEXIT is invisible to guest software; the guest executes zero of its own
  instructions during it.

### Handling the virtual IRQ adds *no* extra VMEXITs
Because the virtual interrupt is delivered through the GIC virtual CPU interface
(List Registers), the guest's acknowledge (`ICC_IAR1_EL1`) and EOI/deactivate
(`ICC_EOIR1_EL1`) are serviced **in hardware at EL1** — they do **not** trap to EL2.

So the real overhead of interrupt virtualization is essentially **one extra
round-trip through EL2** per interrupt. Hardware such as **GICv4/4.1** (vLPIs),
**RISC-V AIA (IMSIC)**, and **x86 APICv / posted interrupts** can deliver certain
interrupts (especially MSIs) *directly* to the guest with no EL2 trap, removing
even that round-trip. ("Support for GICv4" is on hvisor's roadmap.)

---

## 5. Deactivate vs. mask/unmask — two different "forwarding" paths

A common point of confusion: **deactivate (EOI)** and **mask/unmask (enable/disable)**
are *different* operations with *different* mechanisms.

### GIC interrupt lifecycle (this is what "deactivate" refers to)
```
Inactive ──(signaled)──► Pending ──(ack: read IAR)──► Active
   ▲                                                    │
   └──────────────── (deactivate: EOI/DIR) ─────────────┘
```
While an interrupt is **Active**, the GIC will not forward another occurrence of
that same INTID. After the handler runs, it *must* deactivate, or the device can
never interrupt again.

### Deactivate — hardware-accelerated (the "HW-map" bit)
When hvisor injects a hardware interrupt, it writes the List Register with the
**HW bit set** and the physical INTID embedded (`gicv3/mod.rs:304`, `inject_irq`):
```rust
val |= 1 << 61;               // HW = 1: link virtual IRQ -> physical IRQ
val |= (irq_id as u64) << 32; // pINTID
```
When the guest deactivates **its virtual** interrupt, the GIC hardware
**automatically deactivates the linked *physical* interrupt** — with **no trap to
EL2**. This concerns only the active-state lifecycle of a single interrupt
occurrence, so the device is free to fire again.

### Mask/unmask — software-emulated and ownership-restricted
Enabling/disabling an IRQ (`GICD_ISENABLER` / `GICD_ICENABLER`) is a separate,
persistent configuration. The guest writes the **virtual** GICD, which faults out
as an **MMIO trap to EL2** and lands in `restrict_bitmask_access()`
(`src/device/irqchip/gicv3/vgic.rs`). hvisor:
1. Builds an `access_mask` from only the IRQs where `irq_in_zone()` is true, then
2. **writes the value through to the physical GICD** (`mmio_perform_access(gicd_base, ...)`).

So: **yes, masking/unmasking inside a guest does affect the physical IRQ** — but via
vgic MMIO emulation + write-through, *not* via the HW-map bit. Two properties:
- **Restricted by ownership:** the guest can only enable/disable IRQs its zone owns;
  bits outside its ownership are dropped. It cannot touch another zone's interrupts.
- **Costs a VMEXIT:** every mask/unmask is trapped and emulated (rare, so acceptable).

### Summary table
| Guest operation | What it is | Path | Forwarded to physical GIC? | VMEXIT? |
|---|---|---|---|---|
| **EOI / deactivate** (`ICC_EOIR1_EL1`) | complete one interrupt occurrence | GIC virtual CPU interface + **LR HW bit 61** | **Yes, automatically by hardware** | **No** |
| **Mask / unmask** (`GICD_ISENABLER`/`ICENABLER`) | persistent enable/disable config | **MMIO trap → vgic emulation** → write-through | **Yes, by hvisor software**, restricted to owned IRQs | **Yes** |

The design splits the work cleverly: the **frequent** operation (deactivate, once
per interrupt) is offloaded to hardware for zero overhead; the **rare** operation
(masking, plus routing/priority/pending) is trapped and emulated so hvisor can
enforce that a zone only ever affects interrupts it actually owns.

---

## 6. The virtual GIC (vgic)

The guest never touches the real GIC distributor directly. Its accesses hit a
**virtual GIC** emulated by hvisor (`src/device/irqchip/gicv3/vgic.rs`), which is
where `irq_bitmap` ownership is enforced. For example, `vgicv3_handle_irq_ops`
(`vgic.rs:271`) rejects operations on IRQs that are not `is_spi()` or not
`irq_in_zone()` for the calling zone.

This combination — physical routing by ownership + virtual injection + a virtual
distributor that filters by ownership — is what lets each zone manage "its own GIC"
while hvisor guarantees isolation between zones.

---

## 7. Device ownership and interrupts (passthrough)

A passed-through device is owned **exclusively** by one zone, and so is its
interrupt:

| Resource | Owned via | Isolation enforced by |
|----------|-----------|------------------------|
| MMIO registers | mapped into the zone's stage-2 page table (`gpm`) | stage-2 translation (others fault) |
| Interrupt(s) | the zone's `irq_bitmap` + GIC routing | vgic ownership filtering |
| DMA | the zone's IOMMU page table (`iommu_pt`) | IOMMU |

So a USB controller passed through to VM-A is unreachable by VM-B: no MMIO mapping,
the IRQ isn't in VM-B's bitmap, and the IOMMU blocks foreign DMA. Sharing one
physical device across VMs is only possible by first splitting it into multiple
logical pieces — in **software** (a virtio backend in zone0 multiplexes one real
device) or in **hardware** (SR-IOV VFs) — after which each logical piece (and its
IRQ) is again owned by exactly one zone.

---

## 8. Key source references

| Area | File / symbol |
|------|---------------|
| EL2 exception vector, `handle_vmexit` macro | `src/arch/aarch64/trap.S` |
| Exit dispatch (`arch_handle_exit`), IRQ path | `src/arch/aarch64/trap.rs:108`, `irqchip_handle_irq1` |
| Return to guest (`vmreturn` / `ERET`) | `src/arch/aarch64/trap.rs:497` |
| GIC top-level IRQ handler | `src/device/irqchip/mod.rs:36` (`gic_handle_irq`) |
| GICv3 acknowledge + inject loop | `src/device/irqchip/gicv3/mod.rs:97` (`gicv3_handle_irq_el1`) |
| Virtual injection + HW-map bit | `src/device/irqchip/gicv3/mod.rs:304` (`inject_irq`) |
| Virtual GIC distributor / ownership filtering | `src/device/irqchip/gicv3/vgic.rs` |
| Per-zone IRQ ownership | `src/zone.rs:257` (`irq_in_zone`), `zone.rs:908` (`irq_bitmap_init`) |

Other architectures: `src/device/irqchip/{plic,aia,pic,ls7a2000}/`.
