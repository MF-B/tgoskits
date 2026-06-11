use alloc::boxed::Box;
use core::{mem::size_of, ptr::addr_of_mut};

use ax_driver::{PlatformDevice, block::PlatformDeviceBlock, probe::OnProbeError};
use ax_plat::{
    mem::{pa, phys_to_virt, va, virt_to_phys},
    time::{Duration, busy_wait},
};
use rdif_block::{
    BlkError, DeviceInfo, DriverGeneric, IQueue, Interface, QueueInfo, QueueLimits, Request,
    RequestFlags, RequestId, RequestOp, RequestStatus, Segment, validate_request,
};

use crate::config::devices::{AHCI_PADDR, AHCI_PORTS_IMPLEMENTED};

const REG_CAP: usize = 0x00;
const REG_GHC: usize = 0x04;
const REG_IS: usize = 0x08;
const REG_PI: usize = 0x0c;
const REG_VS: usize = 0x10;
const REG_CAP2: usize = 0x24;
const REG_BOHC: usize = 0x28;

const HOST_CAP_SSS: u32 = 0x1 << 27;
const HOST_CAP_MPS: u32 = 0x1 << 28;
const HOST_CTL_RESET: u32 = 0x1 << 0;
const HOST_CTL_AHCI_EN: u32 = 0x1 << 31;

const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;
const PORT_CLB: usize = 0x00;
const PORT_CLBU: usize = 0x04;
const PORT_FB: usize = 0x08;
const PORT_FBU: usize = 0x0c;
const PORT_IS: usize = 0x10;
const PORT_IE: usize = 0x14;
const PORT_CMD: usize = 0x18;
const PORT_TFD: usize = 0x20;
const PORT_SIG: usize = 0x24;
const PORT_SSTS: usize = 0x28;
const PORT_SCTL: usize = 0x2c;
const PORT_SERR: usize = 0x30;
const PORT_CI: usize = 0x38;

const PORT_CMD_ICC_MASK: u32 = 0xf << 28;
const PORT_CMD_ICC_ACTIVE: u32 = 0x1 << 28;
const PORT_CMD_LIST_ON: u32 = 0x1 << 15;
const PORT_CMD_FIS_ON: u32 = 0x1 << 14;
const PORT_CMD_FIS_RX: u32 = 0x1 << 4;
const PORT_CMD_POWER_ON: u32 = 0x1 << 2;
const PORT_CMD_SPIN_UP: u32 = 0x1 << 1;
const PORT_CMD_START: u32 = 0x1 << 0;

const PORT_SCTL_DET_MASK: u32 = 0x0f;
const PORT_SCTL_DET_NONE: u32 = 0x0;
const PORT_SCTL_DET_INIT: u32 = 0x1;

const PORT_TFD_ERR: u32 = 0x1 << 0;
const PORT_TFD_DRQ: u32 = 0x1 << 3;
const PORT_TFD_BSY: u32 = 0x1 << 7;

const AHCI_COMRESET_ASSERT_MILLIS: u64 = 10;
const AHCI_DEVICE_READY_TIMEOUT_MILLIS: usize = 5000;
const AHCI_HBA_RESET_TIMEOUT_MILLIS: usize = 1000;
const AHCI_COMMAND_TIMEOUT_MILLIS: usize = 5000;
const AHCI_LINK_TIMEOUT_MILLIS: usize = 1000;

const AHCI_CMD_LIST_SIZE: usize = 1024;
const AHCI_RX_FIS_SIZE: usize = 256;
const AHCI_IDENTIFY_SIZE: usize = 512;
const AHCI_SECTOR_SIZE: usize = 512;
const AHCI_MAX_TRANSFER_SECTORS: usize = 128;
const AHCI_TRANSFER_BUFFER_SIZE: usize = AHCI_SECTOR_SIZE * AHCI_MAX_TRANSFER_SECTORS;
const AHCI_CMD_TABLE_PRDT_OFFSET: usize = 128;
const AHCI_PRDT_ENTRY_SIZE: usize = 16;
const AHCI_MAX_PRDT_ENTRIES: usize = AHCI_MAX_TRANSFER_SECTORS;
const AHCI_PRDT_BYTE_COUNT_MASK: u32 = 0x3f_ffff;
const AHCI_PRDT_INTERRUPT_ON_COMPLETION: u32 = 1 << 31;
const AHCI_PRDT_MAX_BYTES: usize = AHCI_PRDT_BYTE_COUNT_MASK as usize + 1;
const AHCI_CMD_TABLE_SIZE: usize =
    AHCI_CMD_TABLE_PRDT_OFFSET + AHCI_MAX_PRDT_ENTRIES * AHCI_PRDT_ENTRY_SIZE;
const MBR_PARTITION_TABLE_OFFSET: usize = 446;
const MBR_PARTITION_ENTRY_SIZE: usize = 16;
const MBR_PARTITION_COUNT: usize = 4;
const MBR_PARTITION_TYPE_LINUX: u8 = 0x83;
const MBR_SIGNATURE: u16 = 0xaa55;
const EXT4_SUPERBLOCK_LBA_OFFSET: u64 = 2;
const EXT4_SUPERBLOCK_MAGIC_OFFSET: usize = 0x38;
const EXT4_SUPERBLOCK_MAGIC: u16 = 0xef53;

const SATA_FIS_TYPE_REGISTER_H2D: u8 = 0x27;
const SATA_FIS_H2D_COMMAND: u8 = 0x80;
const ATA_CMD_IDENTIFY_DEVICE: u8 = 0xec;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_FLUSH_CACHE_EXT: u8 = 0xea;
const ATA_DEVICE_LBA: u8 = 0x40;
const AHCI_CMD_SLOT0: u32 = 1;
const DEVICE_NAME: &str = "ls2k1000-ahci";

ax_driver::model_register!(
    name: "LS2K1000 AHCI",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::DEFAULT,
    probe_kinds: &[ProbeKind::Static {
        on_probe: probe_static,
    }],
);

#[repr(C, align(1024))]
struct AhciDma {
    cmd_list: [u8; AHCI_CMD_LIST_SIZE],
    rx_fis: [u8; AHCI_RX_FIS_SIZE],
    cmd_table: [u8; AHCI_CMD_TABLE_SIZE],
    identify: [u8; AHCI_IDENTIFY_SIZE],
    buffer: [u8; AHCI_TRANSFER_BUFFER_SIZE],
}

#[repr(C)]
struct AhciCmdHeader {
    opts: u32,
    status: u32,
    tbl_addr_lo: u32,
    tbl_addr_hi: u32,
    reserved: [u32; 4],
}

#[repr(C)]
struct AhciPrdtEntry {
    addr_lo: u32,
    addr_hi: u32,
    reserved: u32,
    flags_size: u32,
}

struct DmaPtrs {
    cmd_list: *mut u8,
    rx_fis: *mut u8,
    cmd_table: *mut u8,
    identify: *mut u8,
    buffer: *mut u8,
}

#[derive(Clone, Copy)]
struct AhciDmaSegment {
    bus: u64,
    len: usize,
}

impl AhciDmaSegment {
    const fn new(bus: u64, len: usize) -> Self {
        Self { bus, len }
    }
}

struct AtaDmaCommand<'a> {
    command: u8,
    segments: &'a [AhciDmaSegment],
    lba: u64,
    sectors: u16,
    device: u8,
    write: bool,
    label: &'a str,
}

struct AtaNoDataCommand<'a> {
    command: u8,
    label: &'a str,
}

struct AhciController;

#[derive(Clone, Copy)]
struct AhciPort {
    index: usize,
}

struct AhciBlock {
    port: AhciPort,
    capacity_blocks: u64,
    queue_created: bool,
}

struct AhciQueue {
    id: usize,
    port: AhciPort,
    capacity_blocks: u64,
}

#[derive(Clone, Copy)]
struct MbrPartition {
    index: usize,
    boot: u8,
    part_type: u8,
    start_lba: u32,
    sectors: u32,
}

#[derive(Debug)]
enum AhciError {
    InvalidBufferSize,
    LbaOutOfRange,
    CommandFailed,
}

static mut AHCI_DMA: AhciDma = AhciDma {
    cmd_list: [0; AHCI_CMD_LIST_SIZE],
    rx_fis: [0; AHCI_RX_FIS_SIZE],
    cmd_table: [0; AHCI_CMD_TABLE_SIZE],
    identify: [0; AHCI_IDENTIFY_SIZE],
    buffer: [0; AHCI_TRANSFER_BUFFER_SIZE],
};

fn ahci_base() -> *mut u8 {
    phys_to_virt(pa!(AHCI_PADDR)).as_mut_ptr()
}

fn read_reg_u32(offset: usize) -> u32 {
    unsafe { ahci_base().add(offset).cast::<u32>().read_volatile() }
}

fn write_reg_u32(offset: usize, value: u32) {
    unsafe { ahci_base().add(offset).cast::<u32>().write_volatile(value) }
}

fn read_port_reg_u32(port: usize, offset: usize) -> u32 {
    read_reg_u32(PORT_BASE + port * PORT_STRIDE + offset)
}

fn write_port_reg_u32(port: usize, offset: usize, value: u32) {
    write_reg_u32(PORT_BASE + port * PORT_STRIDE + offset, value)
}

fn dma_barrier() {
    unsafe {
        core::arch::asm!("dbar 0");
    }
}

fn dma_ptrs() -> DmaPtrs {
    unsafe {
        let dma = addr_of_mut!(AHCI_DMA);
        DmaPtrs {
            cmd_list: addr_of_mut!((*dma).cmd_list).cast::<u8>(),
            rx_fis: addr_of_mut!((*dma).rx_fis).cast::<u8>(),
            cmd_table: addr_of_mut!((*dma).cmd_table).cast::<u8>(),
            identify: addr_of_mut!((*dma).identify).cast::<u8>(),
            buffer: addr_of_mut!((*dma).buffer).cast::<u8>(),
        }
    }
}

fn clear_dma() -> DmaPtrs {
    let ptrs = dma_ptrs();
    unsafe {
        (addr_of_mut!(AHCI_DMA) as *mut u8).write_bytes(0, size_of::<AhciDma>());
    }
    ptrs
}

fn dma_paddr(ptr: *const u8) -> u64 {
    virt_to_phys(va!(ptr as usize)).as_usize() as u64
}

fn write_addr_pair(port: usize, lo_offset: usize, hi_offset: usize, paddr: u64) {
    write_port_reg_u32(port, lo_offset, paddr as u32);
    write_port_reg_u32(port, hi_offset, (paddr >> 32) as u32);
}

fn port_count(cap: u32) -> usize {
    ((cap & 0x1f) + 1) as usize
}

fn ssts_det(ssts: u32) -> u32 {
    ssts & 0x0f
}

fn ssts_spd(ssts: u32) -> u32 {
    (ssts >> 4) & 0x0f
}

fn ssts_ipm(ssts: u32) -> u32 {
    (ssts >> 8) & 0x0f
}

fn wait_hba_reset_done() -> bool {
    for _ in 0..AHCI_HBA_RESET_TIMEOUT_MILLIS {
        if read_reg_u32(REG_GHC) & HOST_CTL_RESET == 0 {
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }
    false
}

fn reset_hba() {
    let ghc = read_reg_u32(REG_GHC);
    info!("AHCI HBA reset: ghc={ghc:#010x}");

    write_reg_u32(REG_GHC, ghc | HOST_CTL_RESET);
    if !wait_hba_reset_done() {
        warn!("AHCI HBA reset did not complete");
        return;
    }

    let ghc = read_reg_u32(REG_GHC);
    write_reg_u32(REG_GHC, ghc | HOST_CTL_AHCI_EN);
    busy_wait(Duration::from_millis(1));

    info!("AHCI HBA reset done: ghc={:#010x}", read_reg_u32(REG_GHC));
}

fn configure_hba_cap() {
    let cap = read_reg_u32(REG_CAP);
    let new_cap = cap | HOST_CAP_MPS | HOST_CAP_SSS;
    if new_cap == cap {
        return;
    }

    info!("AHCI CAP update: {cap:#010x} -> {new_cap:#010x}");
    write_reg_u32(REG_CAP, new_cap);
    info!("AHCI CAP after update: {:#010x}", read_reg_u32(REG_CAP));
}

fn log_port(port: usize, stage: &str) {
    let cmd = read_port_reg_u32(port, PORT_CMD);
    let tfd = read_port_reg_u32(port, PORT_TFD);
    let sig = read_port_reg_u32(port, PORT_SIG);
    let ssts = read_port_reg_u32(port, PORT_SSTS);
    let sctl = read_port_reg_u32(port, PORT_SCTL);
    let serr = read_port_reg_u32(port, PORT_SERR);

    info!(
        "AHCI port{port} {stage}: cmd={cmd:#010x}, tfd={tfd:#010x}, sig={sig:#010x}, \
         ssts={ssts:#010x}, sctl={sctl:#010x}, serr={serr:#010x}, det={}, spd={}, ipm={}",
        ssts_det(ssts),
        ssts_spd(ssts),
        ssts_ipm(ssts),
    );
}

fn power_up_port(port: usize) {
    let cmd = read_port_reg_u32(port, PORT_CMD);
    if cmd & (PORT_CMD_LIST_ON | PORT_CMD_FIS_ON | PORT_CMD_FIS_RX | PORT_CMD_START) != 0 {
        warn!("AHCI port{port} command engine is already active: cmd={cmd:#010x}");
    }

    let new_cmd =
        (cmd & !PORT_CMD_ICC_MASK) | PORT_CMD_ICC_ACTIVE | PORT_CMD_POWER_ON | PORT_CMD_SPIN_UP;
    write_port_reg_u32(port, PORT_CMD, new_cmd);
}

fn wait_port_link(port: usize, stage: &str) -> bool {
    for _ in 0..AHCI_LINK_TIMEOUT_MILLIS {
        let ssts = read_port_reg_u32(port, PORT_SSTS);
        if ssts_det(ssts) == 0x3 {
            info!(
                "AHCI port{port} link up after {stage}: ssts={ssts:#010x}, spd={}, ipm={}",
                ssts_spd(ssts),
                ssts_ipm(ssts),
            );
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }

    let ssts = read_port_reg_u32(port, PORT_SSTS);
    warn!(
        "AHCI port{port} link not up after {stage} and {AHCI_LINK_TIMEOUT_MILLIS}ms: \
         ssts={ssts:#010x}, det={}, spd={}, ipm={}",
        ssts_det(ssts),
        ssts_spd(ssts),
        ssts_ipm(ssts),
    );
    false
}

fn clear_port_errors(port: usize, stage: &str) {
    let serr = read_port_reg_u32(port, PORT_SERR);
    if serr == 0 {
        return;
    }

    write_port_reg_u32(port, PORT_SERR, serr);
    info!(
        "AHCI port{port} clear SERR after {stage}: {serr:#010x} -> {:#010x}",
        read_port_reg_u32(port, PORT_SERR),
    );
}

fn wait_port_ready(port: usize) -> bool {
    for _ in 0..AHCI_DEVICE_READY_TIMEOUT_MILLIS {
        let tfd = read_port_reg_u32(port, PORT_TFD);
        if tfd & (PORT_TFD_BSY | PORT_TFD_DRQ) == 0 {
            info!("AHCI port{port} device ready: tfd={tfd:#010x}");
            return true;
        }
        busy_wait(Duration::from_millis(1));
    }

    let tfd = read_port_reg_u32(port, PORT_TFD);
    warn!(
        "AHCI port{port} device not ready after {AHCI_DEVICE_READY_TIMEOUT_MILLIS}ms: \
         tfd={tfd:#010x}"
    );
    false
}

fn start_command_engine(port: usize, ptrs: &DmaPtrs) -> bool {
    let cmd_list_paddr = dma_paddr(ptrs.cmd_list);
    let rx_fis_paddr = dma_paddr(ptrs.rx_fis);

    write_addr_pair(port, PORT_CLB, PORT_CLBU, cmd_list_paddr);
    write_addr_pair(port, PORT_FB, PORT_FBU, rx_fis_paddr);
    write_port_reg_u32(port, PORT_IE, 0);
    write_port_reg_u32(port, PORT_IS, u32::MAX);
    write_reg_u32(REG_IS, 1u32 << port);

    let cmd = read_port_reg_u32(port, PORT_CMD);
    let new_cmd = (cmd & !PORT_CMD_ICC_MASK)
        | PORT_CMD_ICC_ACTIVE
        | PORT_CMD_FIS_RX
        | PORT_CMD_POWER_ON
        | PORT_CMD_SPIN_UP
        | PORT_CMD_START;
    write_port_reg_u32(port, PORT_CMD, new_cmd);
    dma_barrier();

    info!(
        "AHCI port{port} command engine started: clb={cmd_list_paddr:#x}, fb={rx_fis_paddr:#x}, \
         cmd={:#010x}",
        read_port_reg_u32(port, PORT_CMD),
    );

    wait_port_ready(port)
}

fn setup_ata_dma_command(
    port: usize,
    ptrs: &DmaPtrs,
    command: AtaDmaCommand<'_>,
) -> Result<(), AhciError> {
    if command.segments.is_empty() || command.segments.len() > AHCI_MAX_PRDT_ENTRIES {
        return Err(AhciError::InvalidBufferSize);
    }
    if command
        .segments
        .iter()
        .any(|segment| segment.len == 0 || segment.len > AHCI_PRDT_MAX_BYTES)
    {
        return Err(AhciError::InvalidBufferSize);
    }

    let cmd_table_paddr = dma_paddr(ptrs.cmd_table);
    let bytes = command
        .segments
        .iter()
        .try_fold(0usize, |total, segment| total.checked_add(segment.len))
        .ok_or(AhciError::InvalidBufferSize)?;
    let first_bus = command.segments[0].bus;

    unsafe {
        ptrs.cmd_table.write_bytes(0, AHCI_CMD_TABLE_SIZE);

        let cfis = ptrs.cmd_table;
        cfis.add(0).write(SATA_FIS_TYPE_REGISTER_H2D);
        cfis.add(1).write(SATA_FIS_H2D_COMMAND);
        cfis.add(2).write(command.command);
        cfis.add(4).write(command.lba as u8);
        cfis.add(5).write((command.lba >> 8) as u8);
        cfis.add(6).write((command.lba >> 16) as u8);
        cfis.add(7).write(command.device);
        cfis.add(8).write((command.lba >> 24) as u8);
        cfis.add(9).write((command.lba >> 32) as u8);
        cfis.add(10).write((command.lba >> 40) as u8);
        cfis.add(12).write(command.sectors as u8);
        cfis.add(13).write((command.sectors >> 8) as u8);

        let write_flag = if command.write { 1 << 6 } else { 0 };
        ptrs.cmd_list.cast::<AhciCmdHeader>().write(AhciCmdHeader {
            opts: ((size_of::<[u8; 20]>() / 4) as u32)
                | write_flag
                | ((command.segments.len() as u32) << 16),
            status: 0,
            tbl_addr_lo: cmd_table_paddr as u32,
            tbl_addr_hi: (cmd_table_paddr >> 32) as u32,
            reserved: [0; 4],
        });

        let prdt = ptrs
            .cmd_table
            .add(AHCI_CMD_TABLE_PRDT_OFFSET)
            .cast::<AhciPrdtEntry>();
        for (index, segment) in command.segments.iter().enumerate() {
            let mut flags_size = (segment.len as u32 - 1) & AHCI_PRDT_BYTE_COUNT_MASK;
            if index + 1 == command.segments.len() {
                flags_size |= AHCI_PRDT_INTERRUPT_ON_COMPLETION;
            }
            prdt.add(index).write(AhciPrdtEntry {
                addr_lo: segment.bus as u32,
                addr_hi: (segment.bus >> 32) as u32,
                reserved: 0,
                flags_size,
            });
        }
    }

    write_port_reg_u32(port, PORT_IS, u32::MAX);
    write_reg_u32(REG_IS, 1u32 << port);
    dma_barrier();

    let label = command.label;
    trace!(
        "AHCI port{port} {label} setup: lba={}, sectors={}, ctba={cmd_table_paddr:#x}, prdt={}, \
         bytes={}, buf={first_bus:#x}",
        command.lba,
        command.sectors,
        command.segments.len(),
        bytes,
    );

    Ok(())
}

fn setup_ata_nodata_command(
    port: usize,
    ptrs: &DmaPtrs,
    command: AtaNoDataCommand<'_>,
) -> Result<(), AhciError> {
    let cmd_table_paddr = dma_paddr(ptrs.cmd_table);

    unsafe {
        ptrs.cmd_table.write_bytes(0, AHCI_CMD_TABLE_SIZE);

        let cfis = ptrs.cmd_table;
        cfis.add(0).write(SATA_FIS_TYPE_REGISTER_H2D);
        cfis.add(1).write(SATA_FIS_H2D_COMMAND);
        cfis.add(2).write(command.command);

        ptrs.cmd_list.cast::<AhciCmdHeader>().write(AhciCmdHeader {
            opts: (size_of::<[u8; 20]>() / 4) as u32,
            status: 0,
            tbl_addr_lo: cmd_table_paddr as u32,
            tbl_addr_hi: (cmd_table_paddr >> 32) as u32,
            reserved: [0; 4],
        });
    }

    write_port_reg_u32(port, PORT_IS, u32::MAX);
    write_reg_u32(REG_IS, 1u32 << port);
    dma_barrier();

    let label = command.label;
    trace!("AHCI port{port} {label} setup: ctba={cmd_table_paddr:#x}");
    Ok(())
}

fn setup_identify_command(port: usize, ptrs: &DmaPtrs) -> Result<(), AhciError> {
    let segments = [AhciDmaSegment {
        bus: dma_paddr(ptrs.identify),
        len: AHCI_IDENTIFY_SIZE,
    }];
    setup_ata_dma_command(
        port,
        ptrs,
        AtaDmaCommand {
            command: ATA_CMD_IDENTIFY_DEVICE,
            segments: &segments,
            lba: 0,
            sectors: 0,
            device: 0,
            write: false,
            label: "IDENTIFY",
        },
    )
}

fn wait_command_done(port: usize) -> bool {
    for _ in 0..AHCI_COMMAND_TIMEOUT_MILLIS {
        if read_port_reg_u32(port, PORT_CI) & AHCI_CMD_SLOT0 == 0 {
            dma_barrier();
            let is = read_port_reg_u32(port, PORT_IS);
            let tfd = read_port_reg_u32(port, PORT_TFD);
            trace!("AHCI port{port} command done: is={is:#010x}, tfd={tfd:#010x}");
            return tfd & (PORT_TFD_BSY | PORT_TFD_DRQ | PORT_TFD_ERR) == 0;
        }
        busy_wait(Duration::from_millis(1));
    }

    warn!(
        "AHCI port{port} command timeout: ci={:#010x}, is={:#010x}, tfd={:#010x}",
        read_port_reg_u32(port, PORT_CI),
        read_port_reg_u32(port, PORT_IS),
        read_port_reg_u32(port, PORT_TFD),
    );
    false
}

fn read_identify_word(ptrs: &DmaPtrs, word: usize) -> u16 {
    unsafe { ptrs.identify.cast::<u16>().add(word).read_volatile() }
}

fn read_identify_string<const N: usize>(ptrs: &DmaPtrs, first_word: usize) -> [u8; N] {
    let mut out = [0; N];
    for i in 0..N / 2 {
        let word = read_identify_word(ptrs, first_word + i);
        out[i * 2] = (word >> 8) as u8;
        out[i * 2 + 1] = word as u8;
    }
    out
}

fn identify_lba28(ptrs: &DmaPtrs) -> u32 {
    read_identify_word(ptrs, 60) as u32 | ((read_identify_word(ptrs, 61) as u32) << 16)
}

fn identify_lba48(ptrs: &DmaPtrs) -> u64 {
    read_identify_word(ptrs, 100) as u64
        | ((read_identify_word(ptrs, 101) as u64) << 16)
        | ((read_identify_word(ptrs, 102) as u64) << 32)
        | ((read_identify_word(ptrs, 103) as u64) << 48)
}

fn identify_capacity(ptrs: &DmaPtrs) -> u64 {
    let lba48 = identify_lba48(ptrs);
    if lba48 != 0 {
        lba48
    } else {
        identify_lba28(ptrs) as u64
    }
}

fn log_identify_data(ptrs: &DmaPtrs) {
    let model = read_identify_string::<40>(ptrs, 27);
    let serial = read_identify_string::<20>(ptrs, 10);
    let model = core::str::from_utf8(&model).unwrap_or("<invalid>");
    let serial = core::str::from_utf8(&serial).unwrap_or("<invalid>");
    let lba28 = identify_lba28(ptrs);
    let lba48 = identify_lba48(ptrs);

    info!(
        "AHCI IDENTIFY: model='{model}', serial='{serial}', lba28={lba28}, lba48={lba48}, \
         word0={:#06x}, word83={:#06x}",
        read_identify_word(ptrs, 0),
        read_identify_word(ptrs, 83),
    );
}

fn identify_device(port: usize, ptrs: &DmaPtrs) -> Option<u64> {
    if let Err(err) = setup_identify_command(port, ptrs) {
        warn!("AHCI port{port} failed to setup IDENTIFY: {err:?}");
        return None;
    }
    write_port_reg_u32(port, PORT_CI, AHCI_CMD_SLOT0);

    if !wait_command_done(port) {
        return None;
    }

    log_identify_data(ptrs);
    Some(identify_capacity(ptrs))
}

fn read_le_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_le_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn sector_signature(sector: &[u8; AHCI_SECTOR_SIZE]) -> u16 {
    ((sector[511] as u16) << 8) | sector[510] as u16
}

fn log_sector(lba: u64, sector: &[u8; AHCI_SECTOR_SIZE]) {
    let sig = sector_signature(sector);
    info!(
        "AHCI LBA{lba}: first16={:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} \
         {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x}, sig={sig:#06x}",
        sector[0],
        sector[1],
        sector[2],
        sector[3],
        sector[4],
        sector[5],
        sector[6],
        sector[7],
        sector[8],
        sector[9],
        sector[10],
        sector[11],
        sector[12],
        sector[13],
        sector[14],
        sector[15],
    );
}

impl MbrPartition {
    fn parse(sector: &[u8; AHCI_SECTOR_SIZE], index: usize) -> Self {
        let offset = MBR_PARTITION_TABLE_OFFSET + index * MBR_PARTITION_ENTRY_SIZE;
        Self {
            index,
            boot: sector[offset],
            part_type: sector[offset + 4],
            start_lba: read_le_u32(sector, offset + 8),
            sectors: read_le_u32(sector, offset + 12),
        }
    }

    const fn is_empty(self) -> bool {
        self.part_type == 0 || self.sectors == 0
    }

    const fn end_lba(self) -> u64 {
        self.start_lba as u64 + self.sectors as u64 - 1
    }
}

fn log_mbr_partitions(sector: &[u8; AHCI_SECTOR_SIZE]) {
    let sig = sector_signature(sector);
    if sig != MBR_SIGNATURE {
        warn!("AHCI MBR: invalid signature {sig:#06x}");
        return;
    }

    for index in 0..MBR_PARTITION_COUNT {
        let partition = MbrPartition::parse(sector, index);
        if partition.is_empty() {
            info!("AHCI MBR partition{index}: empty");
            continue;
        }

        info!(
            "AHCI MBR partition{index}: boot={:#04x}, type={:#04x}, start_lba={}, sectors={}, \
             end_lba={}",
            partition.boot,
            partition.part_type,
            partition.start_lba,
            partition.sectors,
            partition.end_lba(),
        );
    }
}

fn find_linux_partition(sector: &[u8; AHCI_SECTOR_SIZE]) -> Option<MbrPartition> {
    if sector_signature(sector) != MBR_SIGNATURE {
        return None;
    }

    for index in 0..MBR_PARTITION_COUNT {
        let partition = MbrPartition::parse(sector, index);
        if !partition.is_empty() && partition.part_type == MBR_PARTITION_TYPE_LINUX {
            return Some(partition);
        }
    }

    None
}

fn log_ext4_superblock(partition: MbrPartition, sector: &[u8; AHCI_SECTOR_SIZE]) {
    let magic = read_le_u16(sector, EXT4_SUPERBLOCK_MAGIC_OFFSET);
    if magic != EXT4_SUPERBLOCK_MAGIC {
        warn!(
            "AHCI ext4 partition{}: invalid superblock magic {magic:#06x}",
            partition.index,
        );
        return;
    }

    let inodes = read_le_u32(sector, 0x00);
    let blocks = read_le_u32(sector, 0x04);
    let first_data_block = read_le_u32(sector, 0x14);
    let log_block_size = read_le_u32(sector, 0x18);
    let block_size = 1024u32.checked_shl(log_block_size).unwrap_or(0);

    info!(
        "AHCI ext4 partition{}: magic={magic:#06x}, inodes={inodes}, blocks_lo={blocks}, \
         first_data_block={first_data_block}, log_block_size={log_block_size}, \
         block_size={block_size}",
        partition.index,
    );
}

fn ahci_queue_limits() -> QueueLimits {
    QueueLimits {
        supports_flush: true,
        supported_flags: RequestFlags::PREFLUSH | RequestFlags::FUA,
        max_blocks_per_request: AHCI_MAX_TRANSFER_SECTORS as u32,
        max_segments: AHCI_MAX_TRANSFER_SECTORS,
        max_segment_size: AHCI_TRANSFER_BUFFER_SIZE,
        ..QueueLimits::simple(AHCI_SECTOR_SIZE, u64::MAX)
    }
}

fn request_segments_len(segments: &[Segment<'_>]) -> usize {
    segments.iter().map(|segment| segment.len).sum()
}

fn copy_from_request_segments(segments: &[Segment<'_>], offset: usize, dst: &mut [u8]) {
    let mut skipped = offset;
    let mut copied = 0;

    for segment in segments {
        if copied == dst.len() {
            break;
        }
        if skipped >= segment.len {
            skipped -= segment.len;
            continue;
        }

        let start = skipped;
        let len = (segment.len - start).min(dst.len() - copied);
        dst[copied..copied + len].copy_from_slice(&segment[start..start + len]);
        copied += len;
        skipped = 0;
    }
}

fn copy_to_request_segments(segments: &mut [Segment<'_>], offset: usize, src: &[u8]) {
    let mut skipped = offset;
    let mut copied = 0;

    for segment in segments {
        if copied == src.len() {
            break;
        }
        if skipped >= segment.len {
            skipped -= segment.len;
            continue;
        }

        let start = skipped;
        let len = (segment.len - start).min(src.len() - copied);
        segment[start..start + len].copy_from_slice(&src[copied..copied + len]);
        copied += len;
        skipped = 0;
    }
}

fn comreset_port(port: usize) {
    let serr = read_port_reg_u32(port, PORT_SERR);
    if serr != 0 {
        write_port_reg_u32(port, PORT_SERR, serr);
    }

    let sctl = read_port_reg_u32(port, PORT_SCTL);
    info!("AHCI port{port} COMRESET: sctl={sctl:#010x}, serr={serr:#010x}");

    write_port_reg_u32(
        port,
        PORT_SCTL,
        (sctl & !PORT_SCTL_DET_MASK) | PORT_SCTL_DET_INIT,
    );
    busy_wait(Duration::from_millis(AHCI_COMRESET_ASSERT_MILLIS));
    write_port_reg_u32(
        port,
        PORT_SCTL,
        (sctl & !PORT_SCTL_DET_MASK) | PORT_SCTL_DET_NONE,
    );
}

fn bring_up_port_link(port: usize) -> bool {
    power_up_port(port);
    if wait_port_link(port, "power-up") {
        return true;
    }

    comreset_port(port);
    power_up_port(port);
    wait_port_link(port, "COMRESET")
}

fn effective_port_map(raw_pi: u32) -> u32 {
    let board_pi = AHCI_PORTS_IMPLEMENTED as u32;
    if raw_pi != 0 || board_pi == 0 {
        return raw_pi;
    }

    info!("AHCI PI is zero; applying board ports-implemented={board_pi:#010x}");
    write_reg_u32(REG_PI, board_pi);

    let new_pi = read_reg_u32(REG_PI);
    if new_pi == 0 {
        warn!("AHCI PI write did not latch; probing board port map in software");
        board_pi
    } else {
        info!("AHCI PI after board port map write: {new_pi:#010x}");
        new_pi
    }
}

impl AhciController {
    fn probe() -> Option<AhciBlock> {
        let (cap, pi) = Self::init_hba()?;

        for port in 0..port_count(cap).min(32) {
            if pi & (1u32 << port) != 0
                && let Some(block) = AhciPort::new(port).init_block()
            {
                return Some(block);
            }
        }

        warn!("AHCI HBA has no usable ports");
        None
    }

    fn init_hba() -> Option<(u32, u32)> {
        reset_hba();
        configure_hba_cap();

        let cap = read_reg_u32(REG_CAP);
        let ghc = read_reg_u32(REG_GHC);
        let is = read_reg_u32(REG_IS);
        let raw_pi = read_reg_u32(REG_PI);
        let pi = effective_port_map(raw_pi);
        let vs = read_reg_u32(REG_VS);
        let cap2 = read_reg_u32(REG_CAP2);
        let bohc = read_reg_u32(REG_BOHC);

        info!(
            "AHCI HBA: base={AHCI_PADDR:#x}, cap={cap:#010x}, ghc={ghc:#010x}, is={is:#010x}, \
             pi={raw_pi:#010x}, effective_pi={pi:#010x}, vs={vs:#010x}, cap2={cap2:#010x}, \
             bohc={bohc:#010x}"
        );

        if cap == 0 && ghc == 0 && is == 0 && raw_pi == 0 && vs == 0 {
            warn!("AHCI HBA registers read as zero; check AHCI clock/reset and MMIO base");
            return None;
        }
        if cap == u32::MAX
            && ghc == u32::MAX
            && is == u32::MAX
            && raw_pi == u32::MAX
            && vs == u32::MAX
        {
            warn!("AHCI HBA registers read as all ones; check AHCI MMIO mapping");
            return None;
        }
        if pi == 0 {
            warn!("AHCI HBA reports no implemented ports");
            return None;
        }

        Some((cap, pi))
    }
}

impl AhciPort {
    const fn new(index: usize) -> Self {
        Self { index }
    }

    fn init_block(&self) -> Option<AhciBlock> {
        let port = self.index;
        let mut block = None;

        log_port(port, "before-link");
        if bring_up_port_link(port) {
            clear_port_errors(port, "link-up");
            let ptrs = clear_dma();
            if start_command_engine(port, &ptrs)
                && let Some(capacity_blocks) = identify_device(port, &ptrs)
            {
                self.probe_polling_read(&ptrs);
                block = Some(AhciBlock::new(*self, capacity_blocks));
            }
        }
        log_port(port, "after-link");
        block
    }

    fn read_blocks(&self, ptrs: &DmaPtrs, lba: u64, buf: &mut [u8]) -> Result<(), AhciError> {
        if !buf.len().is_multiple_of(AHCI_SECTOR_SIZE) {
            return Err(AhciError::InvalidBufferSize);
        }

        let mut sector_offset = 0;
        for chunk in buf.chunks_mut(AHCI_TRANSFER_BUFFER_SIZE) {
            let Some(chunk_lba) = lba.checked_add(sector_offset) else {
                return Err(AhciError::LbaOutOfRange);
            };
            self.read_dma(ptrs, chunk_lba, chunk)?;
            sector_offset += (chunk.len() / AHCI_SECTOR_SIZE) as u64;
        }

        Ok(())
    }

    fn issue_dma_command(
        &self,
        ptrs: &DmaPtrs,
        command: AtaDmaCommand<'_>,
    ) -> Result<(), AhciError> {
        setup_ata_dma_command(self.index, ptrs, command)?;
        write_port_reg_u32(self.index, PORT_CI, AHCI_CMD_SLOT0);

        if !wait_command_done(self.index) {
            return Err(AhciError::CommandFailed);
        }

        Ok(())
    }

    fn read_dma(&self, ptrs: &DmaPtrs, lba: u64, buf: &mut [u8]) -> Result<(), AhciError> {
        let sectors = buf.len() / AHCI_SECTOR_SIZE;
        let segments = [AhciDmaSegment::new(dma_paddr(ptrs.buffer), buf.len())];
        self.issue_dma_command(
            ptrs,
            AtaDmaCommand {
                command: ATA_CMD_READ_DMA_EXT,
                segments: &segments,
                lba,
                sectors: sectors as u16,
                device: ATA_DEVICE_LBA,
                write: false,
                label: "READ",
            },
        )?;

        let dma_buf = unsafe { core::slice::from_raw_parts(ptrs.buffer as *const u8, buf.len()) };
        buf.copy_from_slice(dma_buf);
        Ok(())
    }

    fn read_request(
        &self,
        ptrs: &DmaPtrs,
        lba: u64,
        sectors: u16,
        segments: &mut [Segment<'_>],
    ) -> Result<(), AhciError> {
        let total_len = sectors as usize * AHCI_SECTOR_SIZE;
        if request_segments_len(segments) != total_len {
            return Err(AhciError::InvalidBufferSize);
        }

        let mut sector_offset = 0usize;
        let mut byte_offset = 0usize;
        while sector_offset < sectors as usize {
            let chunk_sectors = (sectors as usize - sector_offset).min(AHCI_MAX_TRANSFER_SECTORS);
            let chunk_len = chunk_sectors * AHCI_SECTOR_SIZE;
            let chunk_lba = lba
                .checked_add(sector_offset as u64)
                .ok_or(AhciError::LbaOutOfRange)?;
            let dma_segments = [AhciDmaSegment::new(dma_paddr(ptrs.buffer), chunk_len)];

            self.issue_dma_command(
                ptrs,
                AtaDmaCommand {
                    command: ATA_CMD_READ_DMA_EXT,
                    segments: &dma_segments,
                    lba: chunk_lba,
                    sectors: chunk_sectors as u16,
                    device: ATA_DEVICE_LBA,
                    write: false,
                    label: "READ",
                },
            )?;

            let dma_buf =
                unsafe { core::slice::from_raw_parts(ptrs.buffer as *const u8, chunk_len) };
            copy_to_request_segments(segments, byte_offset, dma_buf);
            sector_offset += chunk_sectors;
            byte_offset += chunk_len;
        }

        Ok(())
    }

    fn write_request(
        &self,
        ptrs: &DmaPtrs,
        lba: u64,
        sectors: u16,
        segments: &[Segment<'_>],
    ) -> Result<(), AhciError> {
        let total_len = sectors as usize * AHCI_SECTOR_SIZE;
        if request_segments_len(segments) != total_len {
            return Err(AhciError::InvalidBufferSize);
        }

        let mut sector_offset = 0usize;
        let mut byte_offset = 0usize;
        while sector_offset < sectors as usize {
            let chunk_sectors = (sectors as usize - sector_offset).min(AHCI_MAX_TRANSFER_SECTORS);
            let chunk_len = chunk_sectors * AHCI_SECTOR_SIZE;
            let chunk_lba = lba
                .checked_add(sector_offset as u64)
                .ok_or(AhciError::LbaOutOfRange)?;
            let dma_buf = unsafe { core::slice::from_raw_parts_mut(ptrs.buffer, chunk_len) };
            copy_from_request_segments(segments, byte_offset, dma_buf);
            let dma_segments = [AhciDmaSegment::new(dma_paddr(ptrs.buffer), chunk_len)];

            self.issue_dma_command(
                ptrs,
                AtaDmaCommand {
                    command: ATA_CMD_WRITE_DMA_EXT,
                    segments: &dma_segments,
                    lba: chunk_lba,
                    sectors: chunk_sectors as u16,
                    device: ATA_DEVICE_LBA,
                    write: true,
                    label: "WRITE",
                },
            )?;

            sector_offset += chunk_sectors;
            byte_offset += chunk_len;
        }

        Ok(())
    }

    fn flush_cache(&self, ptrs: &DmaPtrs) -> Result<(), AhciError> {
        setup_ata_nodata_command(
            self.index,
            ptrs,
            AtaNoDataCommand {
                command: ATA_CMD_FLUSH_CACHE_EXT,
                label: "FLUSH",
            },
        )?;
        write_port_reg_u32(self.index, PORT_CI, AHCI_CMD_SLOT0);

        if !wait_command_done(self.index) {
            return Err(AhciError::CommandFailed);
        }

        trace!("AHCI port{} FLUSH done", self.index);
        Ok(())
    }

    fn probe_polling_read(&self, ptrs: &DmaPtrs) {
        let mut sector = [0; AHCI_SECTOR_SIZE];
        let mut linux_partition = None;

        match self.read_blocks(ptrs, 0, &mut sector) {
            Ok(()) => {
                log_sector(0, &sector);
                log_mbr_partitions(&sector);
                linux_partition = find_linux_partition(&sector);
            }
            Err(err) => warn!("AHCI port{} failed to read LBA0: {:?}", self.index, err),
        }

        if let Some(partition) = linux_partition {
            self.probe_ext4_superblock(ptrs, partition);
        }

        match self.read_blocks(ptrs, 1, &mut sector) {
            Ok(()) => log_sector(1, &sector),
            Err(err) => warn!("AHCI port{} failed to read LBA1: {:?}", self.index, err),
        }
    }

    fn probe_ext4_superblock(&self, ptrs: &DmaPtrs, partition: MbrPartition) {
        let mut sector = [0; AHCI_SECTOR_SIZE];
        let superblock_lba = partition.start_lba as u64 + EXT4_SUPERBLOCK_LBA_OFFSET;

        match self.read_blocks(ptrs, superblock_lba, &mut sector) {
            Ok(()) => {
                log_sector(superblock_lba, &sector);
                log_ext4_superblock(partition, &sector);
            }
            Err(err) => warn!(
                "AHCI port{} failed to read ext4 superblock at LBA {}: {:?}",
                self.index, superblock_lba, err,
            ),
        }
    }
}

impl AhciBlock {
    const fn new(port: AhciPort, capacity_blocks: u64) -> Self {
        Self {
            port,
            capacity_blocks,
            queue_created: false,
        }
    }

    fn make_device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: Some(DEVICE_NAME),
            ..DeviceInfo::new(self.capacity_blocks, AHCI_SECTOR_SIZE)
        }
    }
}

impl DriverGeneric for AhciBlock {
    fn name(&self) -> &str {
        DEVICE_NAME
    }
}

impl Interface for AhciBlock {
    fn device_info(&self) -> DeviceInfo {
        self.make_device_info()
    }

    fn queue_limits(&self) -> QueueLimits {
        ahci_queue_limits()
    }

    fn create_queue(&mut self) -> Option<Box<dyn IQueue>> {
        if self.queue_created {
            return None;
        }
        self.queue_created = true;
        Some(Box::new(AhciQueue {
            id: 0,
            port: self.port,
            capacity_blocks: self.capacity_blocks,
        }))
    }
}

impl AhciQueue {
    fn device_info(&self) -> DeviceInfo {
        DeviceInfo {
            name: Some(DEVICE_NAME),
            ..DeviceInfo::new(self.capacity_blocks, AHCI_SECTOR_SIZE)
        }
    }

    fn limits(&self) -> QueueLimits {
        ahci_queue_limits()
    }
}

// SAFETY: AHCI commands complete synchronously in `submit_request`; the queue
// does not retain request segment pointers after the call returns.
unsafe impl IQueue for AhciQueue {
    fn id(&self) -> usize {
        self.id
    }

    fn info(&self) -> QueueInfo {
        QueueInfo {
            id: self.id,
            device: self.device_info(),
            limits: self.limits(),
        }
    }

    fn submit_request(&mut self, request: Request<'_>) -> Result<RequestId, BlkError> {
        validate_request(self.info(), &request)?;
        let ptrs = dma_ptrs();
        let preflush = request.flags.contains(RequestFlags::PREFLUSH);
        let fua = request.flags.contains(RequestFlags::FUA);

        if preflush {
            self.port
                .flush_cache(&ptrs)
                .map_err(|_| BlkError::Other("AHCI preflush failed"))?;
        }

        match request.op {
            RequestOp::Read => {
                self.port
                    .read_request(
                        &ptrs,
                        request.lba,
                        request.block_count as u16,
                        request.segments,
                    )
                    .map_err(|_| BlkError::Other("AHCI read failed"))?;
            }
            RequestOp::Write => {
                self.port
                    .write_request(
                        &ptrs,
                        request.lba,
                        request.block_count as u16,
                        request.segments,
                    )
                    .map_err(|_| BlkError::Other("AHCI write failed"))?;
            }
            RequestOp::Flush => {
                self.port
                    .flush_cache(&ptrs)
                    .map_err(|_| BlkError::Other("AHCI flush failed"))?;
            }
            RequestOp::Discard | RequestOp::WriteZeroes => {
                return Err(BlkError::NotSupported);
            }
        }

        if fua && !matches!(request.op, RequestOp::Flush) {
            self.port
                .flush_cache(&ptrs)
                .map_err(|_| BlkError::Other("AHCI FUA flush failed"))?;
        }
        Ok(RequestId::new(0))
    }

    fn poll_request(&mut self, _request: RequestId) -> Result<RequestStatus, BlkError> {
        Ok(RequestStatus::Complete)
    }
}

fn probe_static(plat_dev: PlatformDevice) -> Result<(), OnProbeError> {
    let Some(block) = AhciController::probe() else {
        return Err(OnProbeError::NotMatch);
    };

    let capacity_blocks = block.capacity_blocks;
    plat_dev.register_block(block);
    info!(
        "registered {DEVICE_NAME} block device: blocks={capacity_blocks}, \
         block_size={AHCI_SECTOR_SIZE}",
    );
    Ok(())
}
