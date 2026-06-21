# hvisor Boot Sequence on x86_64 / QEMU

How control travels from QEMU power-on all the way to `rust_main()` on the
**x86_64 QEMU** target. This is the most involved of hvisor's boot paths because
the x86 CPU starts in a very primitive state (32-bit, paging off) and hvisor
relies on **GRUB + Multiboot2** to load both itself and the root guest.

> Related: [`learn.md`](learn.md) (reading path), [`overview.md`](overview.md)
> (Section 5 has the arch-independent boot/init flow that begins at `rust_main`).

---

## The full chain at a glance

```
QEMU  →  
    OVMF (UEFI firmware)  →  
        GRUB  →  
            arch_entry  →  
                bsp_entry32  →  
                    bsp_entry64  →  
                        rust_entry  →  
                            rust_main()
```

How this compares to the *aarch64* path:


| -   | **aarch64**     | **x86_64**   |
|---|---|---|
| Firmware | BootROM → ATF/BL31 | OVMF (UEFI) |
| Bootloader | U-Boot | **GRUB** (Multiboot2) |
| CPU state at handoff | already 64-bit EL2 | 32-bit protected mode, paging off |
| Boot info passed | DTB pointer in `x0` | Multiboot2 info pointer in `ebx` |
| Guest (zone0) kernel | in DTB / loaded image | GRUB **modules** (`module2`) |

The single most important _x86_ idea: **GRUB loads hvisor as a Multiboot2 kernel,
and loads zone0's Linux kernel + initramfs as Multiboot2 *modules*.** hvisor
later finds those modules and builds zone0 from them — the _x86_ equivalent of
the DTB carrying guest image information.

---

## Step 1 — QEMU + OVMF + GRUB (firmware / bootloader)

From [`platform/x86_64/qemu/platform.mk`](../platform/x86_64/qemu/platform.mk):

```makefile
QEMU_ARGS += -bios /usr/share/ovmf/OVMF.fd            # UEFI firmware (not legacy BIOS)
QEMU_ARGS += -drive file=.../hvisor.iso,...media=disk # the bootable ISO
```

1. QEMU starts **OVMF** (UEFI firmware).
2. OVMF boots from the attached `hvisor.iso`. That ISO is produced with
   `grub-mkrescue` (see the `$(hvisor_bin)` rule in `platform.mk`), so it is an
   EFI-bootable image containing **GRUB**.
3. GRUB reads its menu at
   [`platform/x86_64/qemu/image/iso/boot/grub/grub.cfg`](../platform/x86_64/qemu/image/iso/boot/grub/grub.cfg):

   ```
   menuentry "Hvisor" {
       multiboot2 /boot/hvisor                       # load hvisor via Multiboot2
       module2 /boot/kernel/boot.bin     0
       module2 /boot/kernel/setup.bin    500a000
       module2 /boot/kernel/vmlinux.bin  5100000     # zone0 (root Linux) pieces
       module2 /boot/kernel/initramfs.cpio.gz 1a000000
       boot
   }
   ```

This is why GRUB is required on x86 but not on aarch64: aarch64's U-Boot can
`booti` a kernel directly and hand it a DTB, whereas here hvisor relies on the
**Multiboot2 protocol** (which GRUB implements) to load the hypervisor *and* the
guest images and to report the firmware memory map.

---

## Step 2 — `arch_entry`: GRUB's jump target (32-bit)

Per the Multiboot2 spec, GRUB hands control in **32-bit protected mode with
paging off**. The entry address is declared in the Multiboot2 header
(`tag_entry_address` → `arch_entry`) in
[`src/arch/x86_64/multiboot.S`](../src/arch/x86_64/multiboot.S), and `arch_entry`
itself is `.code32` in
[`src/arch/x86_64/entry.rs`](../src/arch/x86_64/entry.rs):

```rust
#[link_section = ".text.entry"]
pub unsafe extern "C" fn arch_entry() -> i32 {
    asm!("
        .code32
        cli
        mov edi, eax    // Multiboot magic
        mov esi, ebx    // Multiboot info pointer   ← analogous to x0 = DTB on aarch64
        jmp bsp_entry32
    ", ...)
}
```

Multiboot2 passes the magic value in `eax` and the info-structure pointer in
`ebx`; they are saved immediately before any later code clobbers them.

---

## Step 3 — `bsp_entry32`: climb from 32-bit → 64-bit long mode

This stage has no aarch64 counterpart (aarch64 is already in 64-bit EL2). In
[`src/arch/x86_64/multiboot.S`](../src/arch/x86_64/multiboot.S), `bsp_entry32`
runs `ENTRY32_COMMON_1`:

- disable paging, then load a **temporary page table** (`cr3 = .Ltmp_pml4`);
- set PAE + PGE in `CR4`;
- set LME (Long Mode Enable) + NXE in the `IA32_EFER` MSR;
- set paging + protected mode + write-protect in `CR0`.

Enabling paging while LME is set puts the CPU into **long mode**. It then loads a
GDT, sets the data-segment selectors, and does a far return (`retf`) to the
64-bit code selector `0x10`, landing in `bsp_entry64`.

The temporary page table (`.Ltmp_pml4` / `.Ltmp_pdpt_*` at the bottom of
`multiboot.S`) maps only enough to keep executing. Note the `- {offset}` applied
throughout: hvisor is **linked at a high virtual address**
(`offset = 0xffff_ff80_0000_0000`), so this early physical-mode code subtracts
the offset to obtain physical addresses.

---

## Step 4 — `bsp_entry64`: set up the 64-bit environment

Now in long mode, `bsp_entry64`:

- reloads the GDT at its high virtual address;
- loads the TSS (`ltr`);
- clears the segment selectors;
- sets `rsp` to the boot stack top;
- `call`s `rust_entry`.

This is the x86 analog of the stack/MMU setup that aarch64 performs inside
`arch_entry`.

---

## Step 5 — `rust_entry`: first Rust code

[`src/arch/x86_64/entry.rs`](../src/arch/x86_64/entry.rs):

```rust
extern "C" fn rust_entry(magic: u32, info_addr: usize) {
    unsafe { fill_page_table() };          // fill PDPT: map full phys range with huge pages
    crate::clear_bss();                    // zero BSS (Rust statics require this)
    unsafe { PHYS_VIRT_OFFSET = X86_PHYS_VIRT_OFFSET };
    boot::multiboot_init(info_addr);       // parse memory map, modules (zone0 kernel), ACPI
    boot::print_memory_map();
    rust_main(this_apic_id(), info_addr);  // ← into the shared, arch-independent spine
}
```

Functionally this mirrors aarch64's `arch_entry`: prepare a stack, clear BSS,
finish the page tables, then call common code — it just takes more steps because
x86 starts in 32-bit mode and uses Multiboot parsing instead of a DTB.

`boot::multiboot_init()` (in [`src/arch/x86_64/boot.rs`](../src/arch/x86_64/boot.rs))
walks the Multiboot2 tags: the **memory map** (which RAM exists), the
**modules** (the zone0 Linux images GRUB loaded), and **ACPI** tables. That data
is what `rust_main` → `zone_create()` later uses to build zone0.

---

## Step 6 — `rust_main`

Reached via `rust_main(this_apic_id(), info_addr)`:

- `cpuid` = the local APIC id (`this_apic_id()`);
- `host_dtb` = the Multiboot info address, reused as the generic "boot info"
  argument shared with the other arches.

From here execution joins the arch-independent boot/init flow documented in
[`overview.md`](overview.md) Section 5.

### Secondary CPUs (APs)

Application processors come up through a parallel path:
`ap_entry32` → `ap_entry64` → `rust_entry_secondary` → `rust_main(apic_id, 0)`.
They are started later via the INIT–SIPI sequence (not at firmware time), which
is why only the BSP runs the early Multiboot parsing.

---

## Running it (host prerequisites)

To build and boot this target you need on the host:

- `qemu-system-x86_64` with **KVM** — the QEMU args use
  `-accel kvm -cpu host,...,+vmx` (nested VMX);
- **OVMF** firmware at `/usr/share/ovmf/OVMF.fd`;
- `grub-mkrescue` + `xorriso` to build the bootable ISO;
- the zone0 Linux images (`vmlinux.bin`, `setup.bin`, `boot.bin`,
  `initramfs.cpio.gz`) present, or the ISO step warns and skips them.
