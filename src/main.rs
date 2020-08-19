use core::{
    cmp,
    convert::{TryFrom, TryInto},
    mem,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{
    collections::{btree_map, BTreeMap},
    fs,
    io::{self, Write as _},
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{anyhow, bail};
use arrayref::array_ref;
use gimli::{
    read::{CfaRule, DebugFrame, UnwindSection},
    BaseAddresses, EndianSlice, LittleEndian, RegisterRule, UninitializedUnwindContext,
};
use probe_rs::config::registry;
use probe_rs::{
    flashing::{self, Format},
    Core, CoreRegisterAddress, MemoryInterface, Probe, Session,
};
use probe_rs_rtt::{Rtt, ScanRegion, UpChannel};
use structopt::StructOpt;
use xmas_elf::{program::Type, sections::SectionData, symbol_table::Entry, ElfFile};

const TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> Result<(), anyhow::Error> {
    notmain().map(|code| process::exit(code))
}

#[derive(StructOpt)]
#[structopt(name = "probe-run")]
struct Opts {
    #[structopt(long)]
    list_chips: bool,
    #[cfg(feature = "defmt")]
    #[structopt(long)]
    defmt: bool,
    #[structopt(long, required_unless("list-chips"))]
    chip: Option<String>,
    #[structopt(name = "ELF", parse(from_os_str), required_unless("list-chips"))]
    elf: Option<PathBuf>,
}

fn notmain() -> Result<i32, anyhow::Error> {
    env_logger::init();

    let opts: Opts = Opts::from_args();

    if opts.list_chips {
        return print_chips();
    }

    let elf_path = opts.elf.as_deref().unwrap();
    let chip = opts.chip.as_deref().unwrap();
    let bytes = fs::read(elf_path)?;
    let elf = ElfFile::new(&bytes).map_err(|s| anyhow!("{}", s))?;

    #[cfg(feature = "defmt")]
    let table = {
        let table = elf2table::parse(&bytes)?;

        if table.is_none() && opts.defmt {
            bail!(".`.defmt` section not found")
        } else if table.is_some() && !opts.defmt {
            eprintln!("warning: application may be using `defmt` but `--defmt` flag was not used");
        }

        table
    };

    // sections used in cortex-m-rt
    // NOTE we won't load `.uninit` so it is not included here
    // NOTE we don't load `.bss` because the app (cortex-m-rt) will zero it
    let candidates = [".vector_table", ".text", ".rodata"];

    let text = elf
        .section_iter()
        .zip(0..)
        .filter_map(|(sect, shndx)| {
            if sect.get_name(&elf) == Ok(".text") {
                Some(shndx)
            } else {
                None
            }
        })
        .next();

    let mut debug_frame = None;
    let mut range_names = None;
    let mut rtt_addr = None;
    let mut sections = vec![];
    let mut dotdata = None;
    let mut registers = None;
    for sect in elf.section_iter() {
        if let Ok(name) = sect.get_name(&elf) {
            if name == ".debug_frame" {
                debug_frame = Some(sect.raw_data(&elf));
                continue;
            }

            if name == ".symtab" {
                if let Ok(symtab) = sect.get_data(&elf) {
                    let (rn, rtt_addr_) = range_names_from(&elf, symtab, text)?;
                    range_names = Some(rn);
                    rtt_addr = rtt_addr_;
                }
            }

            let size = sect.size();
            // skip empty sections
            if candidates.contains(&name) && size != 0 {
                let start = sect.address();
                if size % 4 != 0 || start % 4 != 0 {
                    // we could support unaligned sections but let's not do that now
                    bail!("section `{}` is not 4-byte aligned", name);
                }

                let start = start.try_into()?;
                let data = sect
                    .raw_data(&elf)
                    .chunks_exact(4)
                    .map(|chunk| u32::from_le_bytes(*array_ref!(chunk, 0, 4)))
                    .collect::<Vec<_>>();

                if name == ".vector_table" {
                    registers = Some(InitialRegisters {
                        vtor: start,
                        // Initial stack pointer
                        sp: data[0],
                        // Reset handler
                        pc: data[1],
                    });
                } else if name == ".data" {
                    // don't put .data in the `sections` variable; it is specially handled
                    dotdata = Some(Data {
                        phys: start,
                        virt: start,
                        data,
                    });
                    continue;
                }

                sections.push(Section { start, data });
            }
        }
    }

    if let Some(data) = dotdata.as_mut() {
        // patch up `.data` physical address
        let mut patched = false;
        for ph in elf.program_iter() {
            if ph.get_type() == Ok(Type::Load) {
                if u32::try_from(ph.virtual_addr())? == data.virt {
                    patched = true;
                    data.phys = ph.physical_addr().try_into()?;
                    break;
                }
            }
        }

        if !patched {
            bail!("couldn't extract `.data` physical address from the ELF");
        }
    }

    let registers = registers.ok_or_else(|| anyhow!("`.vector_table` section is missing"))?;

    let probes = Probe::list_all();
    if probes.is_empty() {
        bail!("no probe was found")
    }
    log::debug!("found {} probes", probes.len());
    let probe = probes[0].open()?;
    log::info!("opened probe");
    let mut sess = probe.attach(chip)?;
    log::info!("started session");

    eprintln!("flashing program ..");

    // load program into memory
    // adjust registers
    // this is the link register reset value; it indicates the end of the call stack
    if registers.vtor >= 0x2000_0000 {
        // program lives in RAM

        let mut core = sess.core(0)?;
        log::info!("attached to core");

        core.reset_and_halt(TIMEOUT)?;
        log::info!("reset and halted the core");

        for section in &sections {
            core.write_32(section.start, &section.data)?;
        }
        if let Some(section) = dotdata {
            core.write_32(section.phys, &section.data)?;
        }

        core.write_core_reg(LR, LR_END)?;
        core.write_core_reg(SP, registers.sp)?;
        core.write_core_reg(PC, registers.pc)?;
        core.write_word_32(VTOR, registers.vtor)?;

        log::info!("loaded program into RAM");

        eprintln!("DONE");

        core.run()?;
    } else {
        // program lives in Flash
        flashing::download_file(&mut sess, elf_path, Format::Elf)?;

        log::info!("flashed program");

        let mut core = sess.core(0)?;
        log::info!("attached to core");

        eprintln!("DONE");
        core.reset()?;
    }

    eprintln!("resetting device");

    static CONTINUE: AtomicBool = AtomicBool::new(true);

    ctrlc::set_handler(|| {
        CONTINUE.store(false, Ordering::Relaxed);
    })?;

    let sess = Arc::new(Mutex::new(sess));
    let mut logging_channel = setup_logging_channel(rtt_addr, sess.clone());

    // wait for breakpoint
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let mut read_buf = [0; 1024];
    #[cfg(feature = "defmt")]
    let mut frames = vec![];
    let mut was_halted = false;
    while CONTINUE.load(Ordering::Relaxed) {
        if let Ok(logging_channel) = &mut logging_channel {
            let num_bytes_read = logging_channel.read(&mut read_buf)?;

            if num_bytes_read != 0 {
                match () {
                    #[cfg(feature = "defmt")]
                    () => {
                        if opts.defmt {
                            frames.extend_from_slice(&read_buf[..num_bytes_read]);

                            while let Ok((frame, consumed)) =
                                decoder::decode(&frames, table.as_ref().unwrap())
                            {
                                writeln!(stdout, "{}", frame.display(true))?;
                                let num_frames = frames.len();
                                frames.rotate_left(consumed);
                                frames.truncate(num_frames - consumed);
                            }
                        } else {
                            stdout.write_all(&read_buf[..num_bytes_read])?;
                        }
                    }
                    #[cfg(not(feature = "defmt"))]
                    () => {
                        stdout.write_all(&read_buf[..num_bytes_read])?;
                    }
                }
            }
        }

        let mut sess = sess.lock().unwrap();
        let mut core = sess.core(0)?;
        let is_halted = core.core_halted()?;

        if is_halted && was_halted {
            break;
        }
        was_halted = is_halted;
    }
    drop(stdout);

    let mut sess = sess.lock().unwrap();
    let mut core = sess.core(0)?;

    // Ctrl-C was pressed; stop the microcontroller
    if !CONTINUE.load(Ordering::Relaxed) {
        core.halt(TIMEOUT)?;
    }

    let pc = core.read_core_reg(PC)?;

    let debug_frame = debug_frame.ok_or_else(|| anyhow!("`.debug_frame` section not found"))?;

    let range_names = range_names.ok_or_else(|| anyhow!("`.symtab` section not found"))?;

    // print backtrace
    let top_exception = backtrace(&mut core, pc, debug_frame, &range_names)?;

    core.reset_and_halt(TIMEOUT)?;

    if let Err(err) = logging_channel {
        return Err(err);
    }

    Ok(if top_exception == Some(TopException::HardFault) {
        SIGABRT
    } else {
        0
    })
}

const SIGABRT: i32 = 134;

#[derive(Debug, PartialEq)]
enum TopException {
    HardFault,
    Other,
}

fn setup_logging_channel(
    rtt_addr: Option<u32>,
    sess: Arc<Mutex<Session>>,
) -> Result<UpChannel, anyhow::Error> {
    if let Some(rtt_addr_res) = rtt_addr {
        const NUM_RETRIES: usize = 5; // picked at random, increase if necessary
        let mut rtt_res: Result<Rtt, probe_rs_rtt::Error> =
            Err(probe_rs_rtt::Error::ControlBlockNotFound);

        for try_index in 0..=NUM_RETRIES {
            rtt_res = Rtt::attach_region(sess.clone(), &ScanRegion::Exact(rtt_addr_res));
            match rtt_res {
                Ok(_) => {
                    log::info!("Successfully attached RTT");
                    break;
                }
                Err(probe_rs_rtt::Error::ControlBlockNotFound) => {
                    if try_index < NUM_RETRIES {
                        log::info!("Could not attach because the target's RTT control block isn't initialized (yet). retrying");
                    } else {
                        log::info!("Max number of RTT attach retries exceeded. Did you call dk::init() first thing in your program?");
                        return Err(anyhow!(probe_rs_rtt::Error::ControlBlockNotFound));
                    }
                }
                Err(e) => {
                    return Err(anyhow!(e));
                }
            }
        }

        let channel = rtt_res
            .expect("unreachable") // this block is only executed when rtt was successfully attached before
            .up_channels()
            .take(0)
            .ok_or_else(|| anyhow!("RTT up channel 0 not found"))?;
        Ok(channel)
    } else {
        Err(anyhow!(
            "No log messages to print, waited for device to halt"
        ))
    }
}

fn gimli2probe(reg: &gimli::Register) -> CoreRegisterAddress {
    CoreRegisterAddress(reg.0)
}

struct Registers<'c, 'probe> {
    cache: BTreeMap<u16, u32>,
    core: &'c mut Core<'probe>,
}

impl<'c, 'probe> Registers<'c, 'probe> {
    fn new(lr: u32, sp: u32, core: &'c mut Core<'probe>) -> Self {
        let mut cache = BTreeMap::new();
        cache.insert(LR.0, lr);
        cache.insert(SP.0, sp);
        Self { cache, core }
    }

    fn get(&mut self, reg: CoreRegisterAddress) -> Result<u32, anyhow::Error> {
        Ok(match self.cache.entry(reg.0) {
            btree_map::Entry::Occupied(entry) => *entry.get(),
            btree_map::Entry::Vacant(entry) => *entry.insert(self.core.read_core_reg(reg)?),
        })
    }

    fn insert(&mut self, reg: CoreRegisterAddress, val: u32) {
        self.cache.insert(reg.0, val);
    }

    fn update_cfa(
        &mut self,
        rule: &CfaRule<EndianSlice<LittleEndian>>,
    ) -> Result</* cfa_changed: */ bool, anyhow::Error> {
        match rule {
            CfaRule::RegisterAndOffset { register, offset } => {
                let cfa = (i64::from(self.get(gimli2probe(register))?) + offset) as u32;
                let ok = self.cache.get(&SP.0) != Some(&cfa);
                self.cache.insert(SP.0, cfa);
                Ok(ok)
            }

            // NOTE not encountered in practice so far
            CfaRule::Expression(_) => todo!("CfaRule::Expression"),
        }
    }

    fn update(
        &mut self,
        reg: &gimli::Register,
        rule: &RegisterRule<EndianSlice<LittleEndian>>,
    ) -> Result<(), anyhow::Error> {
        match rule {
            RegisterRule::Undefined => unreachable!(),

            RegisterRule::Offset(offset) => {
                let cfa = self.get(SP)?;
                let addr = (i64::from(cfa) + offset) as u32;
                self.cache.insert(reg.0, self.core.read_word_32(addr)?);
            }

            _ => unimplemented!(),
        }

        Ok(())
    }
}

fn backtrace(
    core: &mut Core<'_>,
    mut pc: u32,
    debug_frame: &[u8],
    range_names: &RangeNames,
) -> Result<Option<TopException>, anyhow::Error> {
    let mut debug_frame = DebugFrame::new(debug_frame, LittleEndian);
    // 32-bit ARM -- this defaults to the host's address size which is likely going to be 8
    debug_frame.set_address_size(mem::size_of::<u32>() as u8);

    let sp = core.read_core_reg(SP)?;
    let lr = core.read_core_reg(LR)?;

    // statically linked binary -- there are no relative addresses
    let bases = &BaseAddresses::default();
    let ctx = &mut UninitializedUnwindContext::new();

    let mut top_exception = None;
    let mut frame = 0;
    let mut registers = Registers::new(lr, sp, core);
    println!("stack backtrace:");
    loop {
        // FIXME: Add some sanity check for the PC value to avoid unwinding a corrupted stack.
        // For example, PC should always have the LSb (thumb bit) set, and should be located within
        // `.text` (it is possible to execute code from RAM dynamically, but we cannot unwind that).

        let name = range_names
            .binary_search_by(|rn| {
                if rn.0.contains(&pc) {
                    cmp::Ordering::Equal
                } else if pc < rn.0.start {
                    cmp::Ordering::Greater
                } else {
                    cmp::Ordering::Less
                }
            })
            .map(|idx| &*range_names[idx].1)
            .unwrap_or("<unknown>");
        println!("{:>4}: {:#010x} - {}", frame, pc, name);

        let mut cfa_changed = false;
        if let Ok(uwt_row) =
            debug_frame.unwind_info_for_address(bases, ctx, pc.into(), DebugFrame::cie_from_offset)
        {
            cfa_changed = registers.update_cfa(uwt_row.cfa())?;

            for (reg, rule) in uwt_row.registers() {
                registers.update(reg, rule)?;
            }
        }

        // We might not find unwind info for some PC values. In that case, we assume that unwinding
        // is trivial and requires no extra care (ie. we just follow LR).

        let lr = registers.get(LR)?;
        if lr == LR_END {
            break;
        }

        if !cfa_changed && lr == pc {
            // This can happen when we're trying to unwind past the reset handler without having
            // unwind info in place that restores the reset value of LR (0xFFFFFFFF / LR_END).
            return Ok(top_exception);
        }

        if lr > 0xffff_ffe0 {
            let fpu = match lr {
                0xFFFFFFF1 | 0xFFFFFFF9 | 0xFFFFFFFD => false,
                0xFFFFFFE1 | 0xFFFFFFE9 | 0xFFFFFFED => true,
                _ => bail!("LR contains invalid EXC_RETURN value 0x{:08X}", lr),
            };

            // we walk the stack from top (most recent frame) to bottom (oldest frame) so the first
            // exception we see is the top one
            if top_exception.is_none() {
                top_exception = Some(if name == "HardFault" {
                    TopException::HardFault
                } else {
                    TopException::Other
                });
            }
            println!("      <exception entry>");

            let sp = registers.get(SP)?;
            let stacked = Stacked::read(registers.core, sp, fpu)?;

            registers.insert(LR, stacked.lr);
            // adjust the stack pointer for stacked registers
            registers.insert(SP, sp + stacked.size());
            pc = stacked.pc;
        } else {
            if lr & 1 == 0 {
                bail!("bug? LR ({:#010x}) didn't have the Thumb bit set", lr);
            }
            pc = lr;
        }

        frame += 1;
    }

    Ok(top_exception)
}

fn print_chips() -> Result<i32, anyhow::Error> {
    let registry = registry::families().expect("Could not retrieve chip family registry");
    for chip_family in registry {
        println!("{}", chip_family.name);
        println!("    Variants:");
        for variant in chip_family.variants.iter() {
            println!("        {}", variant.name);
        }
    }

    Ok(0)
}

#[derive(Debug)]
struct StackedFpuRegs {
    s0: f32,
    s1: f32,
    s2: f32,
    s3: f32,
    s4: f32,
    s5: f32,
    s6: f32,
    s7: f32,
    s8: f32,
    s9: f32,
    s10: f32,
    s11: f32,
    s12: f32,
    s13: f32,
    s14: f32,
    s15: f32,
    fpscr: u32,
}

/// Registers stacked on exception entry.
#[derive(Debug)]
struct Stacked {
    r0: u32,
    r1: u32,
    r2: u32,
    r3: u32,
    r12: u32,
    lr: u32,
    pc: u32,
    xpsr: u32,
    fpu_regs: Option<StackedFpuRegs>,
}

impl Stacked {
    /// Number of 32-bit words stacked in a basic frame.
    const WORDS_BASIC: usize = 8;

    /// Number of 32-bit words stacked in an extended frame.
    const WORDS_EXTENDED: usize = Self::WORDS_BASIC + 17; // 16 FPU regs + 1 status word

    fn read(core: &mut Core<'_>, sp: u32, fpu: bool) -> Result<Self, anyhow::Error> {
        let mut storage = [0; Self::WORDS_EXTENDED];
        let registers: &mut [_] = if fpu {
            &mut storage
        } else {
            &mut storage[..Self::WORDS_BASIC]
        };
        core.read_32(sp, registers)?;

        Ok(Stacked {
            r0: registers[0],
            r1: registers[1],
            r2: registers[2],
            r3: registers[3],
            r12: registers[4],
            lr: registers[5],
            pc: registers[6],
            xpsr: registers[7],
            fpu_regs: if fpu {
                Some(StackedFpuRegs {
                    s0: f32::from_bits(registers[8]),
                    s1: f32::from_bits(registers[9]),
                    s2: f32::from_bits(registers[10]),
                    s3: f32::from_bits(registers[11]),
                    s4: f32::from_bits(registers[12]),
                    s5: f32::from_bits(registers[13]),
                    s6: f32::from_bits(registers[14]),
                    s7: f32::from_bits(registers[15]),
                    s8: f32::from_bits(registers[16]),
                    s9: f32::from_bits(registers[17]),
                    s10: f32::from_bits(registers[18]),
                    s11: f32::from_bits(registers[19]),
                    s12: f32::from_bits(registers[20]),
                    s13: f32::from_bits(registers[21]),
                    s14: f32::from_bits(registers[22]),
                    s15: f32::from_bits(registers[23]),
                    fpscr: registers[24],
                })
            } else {
                None
            },
        })
    }

    /// Returns the in-memory size of these stacked registers, in Bytes.
    fn size(&self) -> u32 {
        let num_words = if self.fpu_regs.is_none() {
            Self::WORDS_BASIC
        } else {
            Self::WORDS_EXTENDED
        };

        num_words as u32 * 4
    }
}
// FIXME this might already exist in the DWARF data; we should just use that
/// Map from PC ranges to demangled Rust names
type RangeNames = Vec<(Range<u32>, String)>;
type Shndx = u16;

fn range_names_from(
    elf: &ElfFile,
    sd: SectionData,
    text: Option<Shndx>,
) -> Result<(RangeNames, Option<u32>), anyhow::Error> {
    let mut range_names = vec![];
    let mut rtt = None;
    if let SectionData::SymbolTable32(entries) = sd {
        for entry in entries {
            if let Ok(name) = entry.get_name(elf) {
                if name == "_SEGGER_RTT" {
                    rtt = Some(entry.value() as u32);
                }

                if Some(entry.shndx()) == text && entry.size() != 0 {
                    let mut name = rustc_demangle::demangle(name).to_string();
                    // clear the thumb bit
                    let start = entry.value() as u32 & !1;

                    // strip the hash (e.g. `::hd881d91ced85c2b0`)
                    let hash_len = "::hd881d91ced85c2b0".len();
                    if let Some(pos) = name.len().checked_sub(hash_len) {
                        let maybe_hash = &name[pos..];
                        if maybe_hash.starts_with("::h") {
                            // FIXME avoid this allocation
                            name = name[..pos].to_string();
                        }
                    }

                    range_names.push((start..start + entry.size() as u32, name));
                }
            }
        }
    }

    range_names.sort_unstable_by(|a, b| a.0.start.cmp(&b.0.start));

    Ok((range_names, rtt))
}

const LR: CoreRegisterAddress = CoreRegisterAddress(14);
const PC: CoreRegisterAddress = CoreRegisterAddress(15);
const SP: CoreRegisterAddress = CoreRegisterAddress(13);
const VTOR: u32 = 0xE000_ED08;

const LR_END: u32 = 0xFFFF_FFFF;

/// ELF section to be loaded onto the target
#[derive(Debug)]
struct Section {
    start: u32,
    data: Vec<u32>,
}

/// The .data section has a physical address different that its virtual address; we want to load the
/// data into the physical address (which would normally be in FLASH) so that cortex-m-rt doesn't
/// corrupt the variables in .data during initialization -- it would be best if we could disable
/// cortex-m-rt's `init_data` call but there's no such feature
#[derive(Debug)]
struct Data {
    phys: u32,
    virt: u32,
    data: Vec<u32>,
}

/// Registers to update before running the program
struct InitialRegisters {
    sp: u32,
    pc: u32,
    vtor: u32,
}
