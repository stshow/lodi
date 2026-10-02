//! bc-1: kernel parameters, modules and sysctl declared in `host.toml`. `[kernel] parameters`
//! reach the loader's command line for the next boot, `modules` are loaded now and at boot,
//! `blacklist` keeps modules from loading, and `[sysctl]` values are set now and at boot. A
//! value whose declaration is gone goes back to what the distribution had.
//!
//! Every case is the real binary against a scratch `--root` with a **fake machine** behind it
//! (`tests/support/fakehost.rs`): `grub-mkconfig`, `grubby`, `modprobe` and `sysctl` are shims
//! that need the `--root=` lodi passes under a scratch root, and answer from, and change, that
//! root.
//! Nothing reaches `/` and no real tool runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;

use fakehost::{Case, Machine, Pkg, err, nothing, story};

/// A GRUB configuration as a fresh guest has it.
const GRUB: &str = "GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet\"\n\
                    GRUB_CMDLINE_LINUX=\"console=ttyS0\"\n";

/// The loader's configuration `grub-mkconfig` wrote from it.
const GRUB_CFG: &str =
    "menuentry 'Linux' {\n\tlinux /vmlinuz root=/dev/vda1 ro console=ttyS0 quiet\n}\n";

fn machines() -> Vec<(&'static str, Machine)> {
    vec![
        ("debian", Machine::debian()),
        ("arch", Machine::arch()),
        ("fedora", Machine::fedora()),
    ]
}

/// Where the EFI system partition is mounted: `/efi` on Arch, as its image has it.
fn esp(distro: &str) -> &'static str {
    if distro == "arch" { "efi" } else { "boot/efi" }
}

/// A root as a fresh UEFI guest has it: GRUB, the EFI system partition, two sysctl values and
/// one loaded module.
fn case(name: &str, distro: &str, machine: Machine) -> Case {
    let case = Case::new(name, machine);
    case.root.write("etc/default/grub", GRUB);
    std::fs::create_dir_all(case.root.path(&format!("{}/EFI", esp(distro)))).expect("the ESP");
    let grub = if distro == "fedora" { "grub2" } else { "grub" };
    std::fs::create_dir_all(case.root.path(&format!("boot/{grub}"))).expect("the loader's folder");
    if distro == "fedora" {
        case.root.write(
            "boot/loader/entries/fake.conf",
            // As the cloud image has it: not as `grub2-mkconfig` would write it, without `ro`.
            "title Fedora\nlinux /vmlinuz\noptions root=/dev/vda1 console=ttyS0 quiet\n",
        );
        case.root.write(
            "etc/kernel/cmdline",
            "root=/dev/vda1 ro console=ttyS0 quiet\n",
        );
    } else {
        case.root.write("boot/grub/grub.cfg", GRUB_CFG);
    }
    case.root.write("proc/sys/net/ipv4/ip_forward", "0\n");
    case.root.write("proc/sys/vm/swappiness", "60\n");
    case.root.write("sys/module/ext4/refcnt", "1\n");
    case
}

fn manifest(distro: &str, body: &str) -> String {
    format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\n\n{body}")
}

/// The command line the loader boots next: the BLS entry's options on Fedora, else grub.cfg's.
fn next_boot(case: &Case, distro: &str) -> Vec<String> {
    let (file, prefix) = match distro {
        "fedora" => ("boot/loader/entries/fake.conf", "options "),
        _ => ("boot/grub/grub.cfg", "\tlinux /vmlinuz "),
    };
    let text = case.root.read(file);
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("{distro}: no command line in {file}:\n{text}"));
    line.split_whitespace().map(str::to_string).collect()
}

/// The calls of the last command that change something: every kernel tool the fake saw.
fn changes(case: &Case) -> Vec<String> {
    const TOOLS: [&str; 5] = [
        "sysctl",
        "modprobe",
        "grub-mkconfig",
        "grub2-mkconfig",
        "grubby",
    ];
    case.log()
        .into_iter()
        .filter(|line| TOOLS.contains(&line.split(' ').next().unwrap_or("")))
        .collect()
}

fn sysctl(case: &Case, name: &str) -> String {
    case.root
        .read(&format!("proc/sys/{}", name.replace('.', "/")))
        .trim()
        .to_string()
}

#[test]
fn kernel_params_revert() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-params-{distro}"), distro, machine);
        let before = manifest(distro, "");
        case.set_manifest(&before);
        let first = case.apply(&[]);
        assert!(first.status.success(), "{distro}: {}", story(&first));
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());

        case.set_manifest(&manifest(
            distro,
            "[kernel]\nparameters = [\"mitigations=off\", \"lodi.test=1\"]\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        no_trial(&case, distro);
        let cmdline = next_boot(&case, distro);
        for word in ["console=ttyS0", "quiet", "mitigations=off", "lodi.test=1"] {
            assert!(cmdline.contains(&word.to_string()), "{distro}: {cmdline:?}");
        }
        // Fedora's kernel-install gives a kernel installed later its command line from here.
        if distro == "fedora" {
            let later = case.root.read("etc/kernel/cmdline");
            assert!(later.contains("mitigations=off"), "{distro}: {later}");
        }
        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));

        // Deleting one takes it out again, and the other stays.
        case.set_manifest(&manifest(
            distro,
            "[kernel]\nparameters = [\"lodi.test=1\"]\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        no_trial(&case, distro);
        let cmdline = next_boot(&case, distro);
        assert!(
            !cmdline.contains(&"mitigations=off".to_string()),
            "{distro}: {cmdline:?}"
        );
        assert!(
            cmdline.contains(&"lodi.test=1".to_string()),
            "{distro}: {cmdline:?}"
        );

        // The revert: the distribution's own file and command line, as they were.
        case.set_manifest(&before);
        let revert = case.apply(&[]);
        assert!(revert.status.success(), "{distro}: {}", story(&revert));
        assert_eq!(case.root.read("etc/default/grub"), GRUB, "{distro}");
        let own: &[&str] = if distro == "fedora" {
            &["root=/dev/vda1", "console=ttyS0", "quiet"]
        } else {
            &["root=/dev/vda1", "ro", "console=ttyS0", "quiet"]
        };
        assert_eq!(next_boot(&case, distro), own, "{distro}");
        if distro == "fedora" {
            assert_eq!(
                case.root.read("etc/kernel/cmdline"),
                "root=/dev/vda1 ro console=ttyS0 quiet\n",
                "{distro}"
            );
        }
        assert!(
            case.lock().get("basics").is_none(),
            "{distro}: {}",
            case.lock()
        );
    }
}

#[test]
fn kernel_modules_load_blacklist_revert() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-modules-{distro}"), distro, machine);
        let before = manifest(distro, "");
        case.set_manifest(&manifest(
            distro,
            "[kernel]\nmodules = [\"dummy\", \"ext4\"]\nblacklist = [\"pcspkr\"]\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(
            case.root
                .read("etc/modules-load.d/lodi.conf")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            ["dummy", "ext4"],
            "{distro}"
        );
        assert_eq!(
            case.root
                .read("etc/modprobe.d/lodi-blacklist.conf")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            ["blacklist pcspkr"],
            "{distro}"
        );
        // Loaded now: the module that was not, and only that one.
        assert!(case.root.exists("sys/module/dummy"), "{distro}");
        let loads = changes(&case);
        assert_eq!(loads.len(), 1, "{distro}: {loads:?}");
        assert!(
            loads[0].starts_with("modprobe ") && loads[0].ends_with(" dummy"),
            "{loads:?}"
        );

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
        assert!(nothing(&case.plan()), "{distro}");

        // One out of each list, then both tables gone: the distribution's defaults.
        case.set_manifest(&manifest(distro, "[kernel]\nmodules = [\"ext4\"]\n"));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert!(
            case.root
                .read("etc/modules-load.d/lodi.conf")
                .lines()
                .any(|l| l == "ext4")
                && !case
                    .root
                    .read("etc/modules-load.d/lodi.conf")
                    .contains("dummy"),
            "{distro}"
        );
        assert!(
            !case.root.exists("etc/modprobe.d/lodi-blacklist.conf"),
            "{distro}"
        );
        case.set_manifest(&before);
        let revert = case.apply(&[]);
        assert!(revert.status.success(), "{distro}: {}", story(&revert));
        assert!(
            !case.root.exists("etc/modules-load.d/lodi.conf"),
            "{distro}"
        );
        assert!(
            !case.root.exists("etc/modprobe.d/lodi-blacklist.conf"),
            "{distro}"
        );
        assert!(nothing(&case.plan()), "{distro}");
    }
}

#[test]
fn sysctl_apply_revert() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-sysctl-{distro}"), distro, machine);
        let before = manifest(distro, "");
        case.set_manifest(&manifest(
            distro,
            "[sysctl]\n\"net.ipv4.ip_forward\" = \"1\"\n\"vm.swappiness\" = 10\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        // Set now, and in the file systemd-sysctl reads at boot.
        assert_eq!(sysctl(&case, "net.ipv4.ip_forward"), "1", "{distro}");
        assert_eq!(sysctl(&case, "vm.swappiness"), "10", "{distro}");
        assert_eq!(
            case.root
                .read("etc/sysctl.d/90-lodi.conf")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            ["net.ipv4.ip_forward = 1", "vm.swappiness = 10"],
            "{distro}"
        );
        assert_eq!(
            case.lock()["basics"]["sysctl.vm.swappiness"]["was"],
            "60",
            "{distro}"
        );
        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));

        // A removed value returns to what the machine had before lodi set it.
        case.set_manifest(&manifest(
            distro,
            "[sysctl]\n\"net.ipv4.ip_forward\" = \"1\"\n",
        ));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(sysctl(&case, "vm.swappiness"), "60", "{distro}");
        assert_eq!(sysctl(&case, "net.ipv4.ip_forward"), "1", "{distro}");
        assert!(
            !case
                .root
                .read("etc/sysctl.d/90-lodi.conf")
                .contains("swappiness"),
            "{distro}"
        );

        case.set_manifest(&before);
        let revert = case.apply(&[]);
        assert!(revert.status.success(), "{distro}: {}", story(&revert));
        assert_eq!(sysctl(&case, "net.ipv4.ip_forward"), "0", "{distro}");
        assert!(!case.root.exists("etc/sysctl.d/90-lodi.conf"), "{distro}");
        assert!(
            case.lock().get("basics").is_none(),
            "{distro}: {}",
            case.lock()
        );
    }
}

const ALL: &str = "[kernel]\nparameters = [\"mitigations=off\"]\nmodules = [\"dummy\"]\n\
                   blacklist = [\"pcspkr\"]\n\n[sysctl]\n\"net.ipv4.ip_forward\" = \"1\"\n";

#[test]
fn plan_lists_kernel_changes_and_changes_nothing() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-plan-{distro}"), distro, machine);
        case.set_manifest(&manifest(distro, ALL));
        let before = case.machine();
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        for line in [
            "+ parameters mitigations=off (the default from the next boot; ",
            "+ modules dummy (",
            "+ blacklist pcspkr (",
            "~ sysctl net.ipv4.ip_forward 0 -> 1 (",
        ] {
            assert!(text.contains(line), "{distro}: {line:?} in\n{text}");
        }
        assert!(text.contains("host: 4 other changes\n"), "{distro}: {text}");
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(case.machine(), before, "{distro}");
        assert_eq!(case.root.read("etc/default/grub"), GRUB, "{distro}");
        for file in [
            "etc/modules-load.d/lodi.conf",
            "etc/modprobe.d/lodi-blacklist.conf",
            "etc/sysctl.d/90-lodi.conf",
            "sys/module/dummy",
        ] {
            assert!(!case.root.exists(file), "{distro}: {file}");
        }
        assert_eq!(sysctl(&case, "net.ipv4.ip_forward"), "0", "{distro}");
        assert!(!case.has_lock(), "{distro}");

        // After an apply, each changed table is one line of its own, and still nothing changes.
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        case.set_manifest(&manifest(
            distro,
            "[kernel]\nparameters = [\"quiet\"]\nmodules = [\"dummy\"]\n\n\
             [sysctl]\n\"net.ipv4.ip_forward\" = \"0\"\n",
        ));
        let grub = case.root.read("etc/default/grub");
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        for line in [
            "~ parameters mitigations=off -> quiet (",
            "- blacklist pcspkr (",
            "~ sysctl net.ipv4.ip_forward 1 -> 0 (",
        ] {
            assert!(text.contains(line), "{distro}: {line:?} in\n{text}");
        }
        assert!(!text.contains(" modules "), "{distro}: {text}");
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(case.root.read("etc/default/grub"), grub, "{distro}");
        assert!(
            case.root.exists("etc/modprobe.d/lodi-blacklist.conf"),
            "{distro}"
        );
        assert_eq!(sysctl(&case, "net.ipv4.ip_forward"), "1", "{distro}");
    }
}

#[test]
fn a_value_the_machine_cannot_take_is_refused_before_any_change() {
    let (distro, machine) = machines().remove(0);
    let case = case("boot-unknown", distro, machine);
    case.set_manifest(&manifest(
        distro,
        "[kernel]\nmodules = [\"dummy\"]\n\n[sysctl]\n\"net.no.such\" = \"1\"\n",
    ));
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(3), "{}", story(&apply));
    for word in ["E_UNKNOWN_SETTING", "net.no.such"] {
        assert!(err(&apply).contains(word), "{word}: {}", story(&apply));
    }
    assert!(changes(&case).is_empty(), "{:?}", case.log());
    assert!(!case.root.exists("etc/modules-load.d/lodi.conf"));
}

#[test]
fn parameters_without_a_loader_lodi_knows_are_refused_before_any_change() {
    let (distro, machine) = machines().remove(0);
    let case = Case::new("boot-no-loader", machine);
    case.set_manifest(&manifest(distro, "[kernel]\nparameters = [\"quiet\"]\n"));
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(7), "{}", story(&apply));
    assert!(err(&apply).contains("E_BOOT_LOADER"), "{}", story(&apply));
    assert!(changes(&case).is_empty(), "{:?}", case.log());
}

#[test]
fn the_kernel_tables_take_their_grammar() {
    let (distro, machine) = machines().remove(0);
    let case = case("boot-grammar", distro, machine);
    for body in [
        "[kernel]\nparameters = [\"a b\"]\n",
        "[kernel]\nparameters = [\"x=\\\"$(id)\\\"\"]\n",
        "[kernel]\nmodules = [\"-r\"]\n",
        "[kernel]\nblacklist = [\"../x\"]\n",
        "[kernel]\ncmdline = [\"quiet\"]\n",
        "[sysctl]\n\"-a\" = \"1\"\n",
        "[sysctl]\n\"net..x\" = \"1\"\n",
        "[sysctl]\n\"vm.swappiness\" = \"1\\n2\"\n",
        "[sysctl]\n\"vm.swappiness\" = true\n",
    ] {
        case.set_manifest(&manifest(distro, body));
        let plan = case.plan();
        assert_eq!(plan.status.code(), Some(3), "{body}: {}", story(&plan));
        assert!(changes(&case).is_empty(), "{body}");
    }
}

#[test]
fn package_exact_mode_keeps_the_running_kernel_and_the_bootloader() {
    let (distro, machine) = machines().remove(0);
    let machine = machine
        .with(Pkg::new("grub-efi-amd64"))
        .with(Pkg::new("shim-signed"))
        .with(Pkg::new("efibootmgr"))
        .with(Pkg::new("htop"));
    let case = case("boot-exact", distro, machine);
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"{distro}\"\npackages = \"exact\"\n\n\
         [packages]\ncommon = [\"bash\"]\n\n[kernel]\nparameters = [\"quiet\"]\n"
    ));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let text = err(&plan);
    assert!(
        text.contains("htop"),
        "exact mode still removes the rest: {text}"
    );
    for name in ["grub-efi-amd64", "shim-signed", "efibootmgr", "linux-image"] {
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with('-') && line.contains(name)),
            "{name}: {text}"
        );
    }
}

#[test]
fn a_reimport_keeps_the_kernel_tables() {
    let (distro, machine) = machines().remove(0);
    let case = case("boot-reimport", distro, machine);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let imported = fakehost::without_snapshot(&case.manifest());
    case.set_manifest(&format!("{imported}\n{ALL}"));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(case.manifest().contains(ALL), "{}", case.manifest());
}

// bl-1 (LD-424): the bootloader declared in `[boot]`, installed and switched on a UEFI root.

/// The firmware's variables, as efivarfs shows them under the root.
const EFIVARS: &str = "sys/firmware/efi/efivars";
const EFI_GLOBAL: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const SD_BOOT_PATH: &str = "\\EFI\\systemd\\systemd-bootx64.efi";

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// One `Boot####` variable: an active load option whose device path is one node.
fn load_option(label: &str, node: &[u8]) -> Vec<u8> {
    let mut path = node.to_vec();
    path.extend([0x7f, 0xff, 4, 0]);
    let mut data = 7u32.to_le_bytes().to_vec();
    data.extend(1u32.to_le_bytes());
    data.extend((path.len() as u16).to_le_bytes());
    data.extend(utf16(label));
    data.extend(path);
    data
}

fn file_node(loader: &str) -> Vec<u8> {
    let path = utf16(loader);
    let mut node = vec![4, 4];
    node.extend(((4 + path.len()) as u16).to_le_bytes());
    node.extend(path);
    node
}

/// The loader, the ESP, the kernel and the firmware's own entries of a fresh UEFI guest.
fn uefi(name: &str, distro: &str, machine: Machine) -> (Case, &'static str) {
    let machine = match distro {
        "debian" => machine.offering(Pkg::new("systemd-boot")),
        "fedora" => machine.offering(Pkg::new("systemd-boot-unsigned")),
        _ => machine,
    };
    let case = case(name, distro, machine);
    let (esp, kver, partition) = match distro {
        "arch" => ("efi", "7.2.6-arch2-1", "2"),
        "fedora" => ("boot/efi", "6.19.10-300.fc44.x86_64", "2"),
        _ => ("boot/efi", "6.1.0-25-amd64", "15"),
    };
    let root = &case.root;
    root.write(&format!("{EFIVARS}/.keep"), "");
    let var = |name: &str, data: &[u8]| {
        std::fs::write(root.path(&format!("{EFIVARS}/{name}-{EFI_GLOBAL}")), data)
            .expect("a variable")
    };
    // The firmware's setup screen and its entry for the disk's removable path.
    var(
        "Boot0000",
        &load_option(
            "UiApp",
            &[
                4, 6, 20, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
            ],
        ),
    );
    var(
        "Boot0001",
        &load_option("UEFI Misc Device", &[1, 1, 6, 0, 0, 4]),
    );
    let mut order = vec![0u16, 1];
    match distro {
        "arch" => {
            root.write(&format!("{esp}/EFI/BOOT/BOOTX64.EFI"), "GRUB removable");
            root.write(&format!("usr/lib/modules/{kver}/vmlinuz"), "kernel");
            root.write("usr/lib/systemd/boot/efi/systemd-bootx64.efi", "sd-boot");
        }
        _ => {
            let dir = if distro == "fedora" {
                "fedora"
            } else {
                "debian"
            };
            root.write(&format!("{esp}/EFI/{dir}/shimx64.efi"), "SHIM");
            root.write(&format!("{esp}/EFI/{dir}/grubx64.efi"), "GRUB");
            root.write(&format!("{esp}/EFI/BOOT/BOOTX64.EFI"), "SHIM removable");
            if distro == "fedora" {
                let loader = format!("\\EFI\\{dir}\\shimx64.efi");
                var("Boot0008", &load_option("Fedora", &file_node(&loader)));
                order.insert(0, 8);
                root.write(&format!("usr/lib/modules/{kver}/vmlinuz"), "kernel");
                root.write(
                    "etc/kernel/cmdline",
                    "root=UUID=f00 ro rootflags=subvol=root console=ttyS0\n",
                );
            } else {
                root.write(&format!("boot/vmlinuz-{kver}"), "kernel");
                std::fs::create_dir_all(root.path(&format!("usr/lib/modules/{kver}")))
                    .expect("the modules");
                // Debian's cloud image sets its timeout in a drop-in.
                root.write("etc/default/grub.d/15_timeout.cfg", "GRUB_TIMEOUT=1\n");
            }
        }
    }
    let order: Vec<u8> = 7u32
        .to_le_bytes()
        .into_iter()
        .chain(order.iter().flat_map(|n| n.to_le_bytes()))
        .collect();
    var("BootOrder", &order);
    let device = format!("vda{partition}");
    // Arch mounts its ESP on demand: an autofs line comes first.
    let autofs = if distro == "arch" {
        format!("35 25 0:40 / /{esp} rw,relatime - autofs systemd-1 rw\n")
    } else {
        String::new()
    };
    root.write(
        "proc/self/mountinfo",
        &format!("{autofs}36 25 254:3 / /{esp} rw,relatime - vfat /dev/{device} rw\n"),
    );
    root.write(
        &format!("sys/class/block/{device}/partition"),
        &format!("{partition}\n"),
    );
    root.write("etc/machine-id", "0123456789abcdef0123456789abcdef\n");
    root.write(
        "proc/cmdline",
        "BOOT_IMAGE=/boot/vmlinuz root=/dev/vda1 ro console=ttyS0\n",
    );
    (case, esp)
}

/// The boot order, and each entry's label and file path.
fn nvram(case: &Case) -> (Vec<u16>, BTreeMap<u16, (String, String)>) {
    let dir = case.root.path(EFIVARS);
    let words = |data: &[u8]| -> Vec<u16> {
        data.chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect()
    };
    let order = words(&std::fs::read(dir.join(format!("BootOrder-{EFI_GLOBAL}"))).unwrap()[4..]);
    let mut entries = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(number) = name
            .strip_prefix("Boot")
            .and_then(|rest| u16::from_str_radix(rest.get(..4)?, 16).ok())
        else {
            continue;
        };
        let data = std::fs::read(entry.path()).unwrap();
        let body = words(&data[10..]);
        let end = body.iter().position(|w| *w == 0).unwrap();
        let label = String::from_utf16(&body[..end]).unwrap();
        let mut at = 10 + 2 * (end + 1);
        let mut path = String::new();
        while at + 4 <= data.len() && data[at] != 0x7f {
            let len = u16::from_le_bytes([data[at + 2], data[at + 3]]) as usize;
            if data[at] == 4 && data[at + 1] == 4 {
                let text = words(&data[at + 4..at + len]);
                path = String::from_utf16(&text[..text.len() - 1]).unwrap();
            }
            at += len;
        }
        entries.insert(number, (label, path));
    }
    (order, entries)
}

/// The file path of the entry the firmware tries `nth`.
fn boots(case: &Case, nth: usize) -> String {
    let (order, entries) = nvram(case);
    entries[&order[nth]].1.clone()
}

/// The path GRUB's own entry names on each machine.
fn grub_path(distro: &str) -> &'static str {
    match distro {
        "arch" => "\\EFI\\BOOT\\BOOTX64.EFI",
        "fedora" => "\\EFI\\fedora\\shimx64.efi",
        _ => "\\EFI\\debian\\shimx64.efi",
    }
}

/// Every file on the ESP, with its bytes.
fn esp_files(case: &Case, esp: &str) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &std::path::Path, base: &std::path::Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    let base = case.root.path(esp);
    walk(&base, &base, &mut out);
    out
}

/// The calls of the last command to the bootloader's tools and the package managers.
fn boot_changes(case: &Case) -> Vec<String> {
    const TOOLS: [&str; 7] = [
        "efibootmgr",
        "bootctl",
        "kernel-install",
        "grub-install",
        "grub-mkconfig",
        "grub2-mkconfig",
        "apt-get",
    ];
    case.log()
        .into_iter()
        .filter(|line| TOOLS.contains(&line.split(' ').next().unwrap_or("")))
        .filter(|line| !line.contains(" update"))
        .collect()
}

/// The value of the last `KEY VALUE` line of systemd-boot's `loader.conf`.
fn loader_conf(case: &Case, esp: &str, key: &str) -> Option<String> {
    case.root
        .read(&format!("{esp}/loader/loader.conf"))
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(&format!("{key} ")))
        .map(str::to_string)
}

const SYSTEMD_BOOT: &str = "[kernel]\nparameters = [\"lodi.test=1\"]\n\n\
                            [boot]\nloader = \"systemd-boot\"\ndefault = \"lodi-*\"\ntimeout = 3\n";
const BACK_TO_GRUB: &str = "[kernel]\nparameters = [\"lodi.test=1\"]\n\n\
                            [boot]\nloader = \"grub\"\ndefault = \"saved\"\ntimeout = 7\n";

/// Switch a fresh root to systemd-boot: the next start is systemd-boot at once, GRUB right
/// after it in the boot order as the fallback, and nothing starts once (LD-491).
fn to_systemd_boot(case: &Case, distro: &str) {
    case.set_manifest(&manifest(distro, SYSTEMD_BOOT));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{distro}: {}", story(&apply));
    no_trial(case, distro);
    assert_eq!(boots(case, 0), SD_BOOT_PATH, "{distro}: {:?}", nvram(case));
    assert_eq!(
        boots(case, 1),
        grub_path(distro),
        "{distro}: {:?}",
        nvram(case)
    );
    assert_eq!(
        power_on(case, distro, esp(distro)).0,
        SD_BOOT_PATH,
        "{distro}"
    );
}

#[test]
fn loader_grub_to_systemd_boot() {
    for (distro, machine) in machines() {
        let (case, esp) = uefi(&format!("loader-sd-{distro}"), distro, machine);
        let before_esp = esp_files(&case, esp);
        let before_nvram = nvram(&case);
        case.set_manifest(&manifest(distro, SYSTEMD_BOOT));
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        for line in [
            "~ loader grub -> systemd-boot (",
            "+ timeout 3 (",
            "+ default lodi-* (",
        ] {
            assert!(text.contains(line), "{distro}: {line:?} in\n{text}");
        }
        assert!(boot_changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(esp_files(&case, esp), before_esp, "{distro}");
        assert_eq!(nvram(&case), before_nvram, "{distro}");

        to_systemd_boot(&case, distro);
        // The firmware's removable path still starts the loader it started before.
        assert_eq!(
            esp_files(&case, esp)["EFI/BOOT/BOOTX64.EFI"],
            before_esp["EFI/BOOT/BOOTX64.EFI"],
            "{distro}"
        );
        assert_eq!(loader_conf(&case, esp, "timeout").as_deref(), Some("3"));
        assert_eq!(
            loader_conf(&case, esp, "default").as_deref(),
            Some("lodi-*")
        );
        let entries: Vec<String> = esp_files(&case, esp)
            .into_iter()
            .filter(|(path, _)| path.starts_with("loader/entries/"))
            .map(|(_, bytes)| String::from_utf8(bytes).unwrap())
            .collect();
        assert_eq!(entries.len(), 1, "{distro}: {entries:?}");
        let options = entries[0]
            .lines()
            .find_map(|line| line.strip_prefix("options "))
            .unwrap_or_default();
        for word in ["ro", "console=ttyS0", "lodi.test=1"] {
            assert!(options.split(' ').any(|w| w == word), "{distro}: {options}");
        }
        assert!(!options.contains("BOOT_IMAGE"), "{distro}: {options}");

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(
            boot_changes(&case).is_empty(),
            "{distro}: {}",
            story(&again)
        );
        assert!(nothing(&case.plan()), "{distro}");
    }
}

#[test]
fn loader_systemd_boot_to_grub() {
    for (distro, machine) in machines() {
        let (case, esp) = uefi(&format!("loader-grub-{distro}"), distro, machine);
        to_systemd_boot(&case, distro);

        case.set_manifest(&manifest(distro, BACK_TO_GRUB));
        let plan = case.plan();
        let text = err(&plan);
        assert!(
            text.contains("~ loader systemd-boot -> grub ("),
            "{distro}: {}",
            story(&plan)
        );
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        // Back to GRUB at once, systemd-boot right after it as the fallback.
        no_trial(&case, distro);
        assert_eq!(
            boots(&case, 0),
            grub_path(distro),
            "{distro}: {:?}",
            nvram(&case)
        );
        assert_eq!(
            boots(&case, 1),
            SD_BOOT_PATH,
            "{distro}: {:?}",
            nvram(&case)
        );
        assert_eq!(
            power_on(&case, distro, esp).0,
            grub_path(distro),
            "{distro}"
        );
        let grub = if distro == "fedora" {
            "boot/grub2/grub.cfg"
        } else {
            "boot/grub/grub.cfg"
        };
        let config = case.root.read(grub);
        assert!(config.contains("set timeout=7\n"), "{distro}: {config}");
        assert!(
            config.contains("set default=\"saved\"\n"),
            "{distro}: {config}"
        );
        assert!(config.contains("lodi.test=1"), "{distro}: {config}");
        // systemd-boot keeps its entry, its files and its settings, as the fallback.
        assert!(esp_files(&case, esp).contains_key("EFI/systemd/systemd-bootx64.efi"));
        assert_eq!(loader_conf(&case, esp, "timeout").as_deref(), Some("3"));

        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(
            boot_changes(&case).is_empty(),
            "{distro}: {}",
            story(&again)
        );
        assert!(nothing(&case.plan()), "{distro}");
    }
}

/// Ubuntu 24.04's cloud image mounts `/boot` from an ext4 XBOOTLDR partition. `kernel-install`
/// writes its entries there, where the firmware cannot read them, unless it is told the ESP.
#[test]
fn loader_systemd_boot_entries_on_the_esp_past_an_ext4_boot() {
    let (case, esp) = uefi("loader-xbootldr", "debian", Machine::debian());
    let mounts = case.root.read("proc/self/mountinfo");
    case.root.write(
        "proc/self/mountinfo",
        &format!("35 25 254:16 / /boot rw,relatime - ext4 /dev/vda16 rw\n{mounts}"),
    );
    to_systemd_boot(&case, "debian");
    let conf = case.root.read("etc/kernel/install.conf");
    for line in ["layout=bls", "BOOT_ROOT=/boot/efi"] {
        assert!(conf.lines().any(|l| l == line), "{line:?} in {conf}");
    }
    assert!(
        esp_files(&case, esp)
            .keys()
            .any(|path| path.starts_with("loader/entries/")),
        "{:?}",
        esp_files(&case, esp).keys()
    );
    let again = case.apply(&[]);
    assert!(again.status.success(), "{}", story(&again));
    assert!(boot_changes(&case).is_empty(), "{}", story(&again));

    // Back to GRUB: kernel-install writes where the distribution had it write.
    case.set_manifest(&manifest("debian", BACK_TO_GRUB));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    no_trial(&case, "debian");
    assert_eq!(boots(&case, 0), grub_path("debian"), "{:?}", nvram(&case));
    assert_eq!(power_on(&case, "debian", esp).0, grub_path("debian"));
    assert!(!case.root.path("etc/kernel/install.conf").exists());
    assert!(nothing(&case.plan()));
}

#[test]
fn loader_fallback_after_failure() {
    for (distro, machine) in machines() {
        let (case, esp) = uefi(&format!("loader-fail-{distro}"), distro, machine);
        case.edit(|machine| {
            machine["fail"] = serde_json::json!({
                "kernel-install add": "kernel-install: cannot write the entry: No space left on device"
            });
        });
        let before_esp = esp_files(&case, esp);
        let before_nvram = nvram(&case);
        case.set_manifest(&manifest(distro, SYSTEMD_BOOT));
        let apply = case.apply(&[]);
        assert_eq!(apply.status.code(), Some(8), "{distro}: {}", story(&apply));
        for word in ["E_APPLY", "kernel-install", "No space left on device"] {
            assert!(
                err(&apply).contains(word),
                "{distro}: {word}: {}",
                story(&apply)
            );
        }
        // The earlier loader is still what the firmware starts, from the same files.
        assert_eq!(
            boots(&case, 0),
            before_nvram.1[&before_nvram.0[0]].1,
            "{distro}"
        );
        assert_eq!(nvram(&case).0, before_nvram.0, "{distro}");
        assert_eq!(esp_files(&case, esp), before_esp, "{distro}");
        assert!(!case.has_lock(), "{distro}: nothing was recorded");

        // Once the machine can take it, the same apply goes through.
        case.edit(|machine| {
            machine.as_object_mut().unwrap().remove("fail");
        });
        to_systemd_boot(&case, distro);
    }
}

#[test]
fn loader_refuses_bios() {
    for (distro, machine) in machines() {
        let (case, esp) = uefi(&format!("loader-bios-{distro}"), distro, machine);
        std::fs::remove_dir_all(case.root.path("sys/firmware/efi")).expect("no UEFI");
        let before_esp = esp_files(&case, esp);
        let before = case.machine();
        case.set_manifest(&manifest(distro, SYSTEMD_BOOT));
        for output in [case.plan(), case.apply(&[])] {
            assert_eq!(
                output.status.code(),
                Some(7),
                "{distro}: {}",
                story(&output)
            );
            for word in ["E_BOOT_NOT_UEFI", "UEFI"] {
                assert!(err(&output).contains(word), "{distro}: {}", story(&output));
            }
        }
        assert!(boot_changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(esp_files(&case, esp), before_esp, "{distro}");
        assert_eq!(case.root.read("etc/default/grub"), GRUB, "{distro}");
        assert_eq!(case.machine(), before, "{distro}");
        assert!(!case.has_lock(), "{distro}");
    }
}

#[test]
fn loader_removal_restores_defaults() {
    for (distro, machine) in machines() {
        let (case, esp) = uefi(&format!("loader-removal-{distro}"), distro, machine);
        let grub_file = case.root.read("etc/default/grub");
        let grub_d = |case: &Case| {
            std::fs::read_dir(case.root.path("etc/default/grub.d"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        };
        let before_d = grub_d(&case);
        case.set_manifest(&manifest(distro, BACK_TO_GRUB));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        to_systemd_boot(&case, distro);
        let sd_entry = nvram(&case).0[0];

        // The declaration is gone: the distribution's loader and settings, both entries kept.
        case.set_manifest(&manifest(distro, ""));
        let plan = case.plan();
        assert!(
            err(&plan).contains("~ loader systemd-boot -> grub ("),
            "{distro}: {}",
            story(&plan)
        );
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert_eq!(
            boots(&case, 0),
            grub_path(distro),
            "{distro}: {:?}",
            nvram(&case)
        );
        let (order, entries) = nvram(&case);
        assert_eq!(order[1], sd_entry, "{distro}: the known-good entry stays");
        assert_eq!(entries[&sd_entry].1, SD_BOOT_PATH, "{distro}");
        assert_eq!(case.root.read("etc/default/grub"), grub_file, "{distro}");
        assert_eq!(grub_d(&case), before_d, "{distro}");
        assert_eq!(
            loader_conf(&case, esp, "timeout").as_deref(),
            None,
            "{distro}"
        );
        assert_eq!(
            loader_conf(&case, esp, "default").as_deref(),
            None,
            "{distro}"
        );
        assert!(
            case.lock().get("basics").is_none_or(|b| b
                .as_object()
                .unwrap()
                .keys()
                .all(|k| !k.starts_with("boot."))),
            "{distro}: {}",
            case.lock()
        );
        assert!(nothing(&case.plan()), "{distro}");
    }
}

// A boot change takes effect at once (LD-491, #686): a change to `[kernel] parameters` or
// `[boot] loader` is the default from the next boot, and the entry that was the default stays
// in the menu as the fallback, picked by hand. Nothing boots once and nothing waits to be
// confirmed.

/// `grub.cfg`, as `grub-mkconfig` writes it.
fn grub_cfg(distro: &str) -> &'static str {
    if distro == "fedora" {
        "boot/grub2/grub.cfg"
    } else {
        "boot/grub/grub.cfg"
    }
}

/// The command line of the entry `id`, a `grub.cfg` menu entry.
fn entry(case: &Case, distro: &str, id: &str) -> Option<Vec<String>> {
    let words = |line: &str| line.split_whitespace().map(str::to_string).collect();
    let text = case.root.read(grub_cfg(distro));
    let mut lines = text.lines().skip_while(|line| {
        !(line.starts_with("menuentry ") && line.contains(&format!("--id {id} ")))
    });
    lines.next()?;
    lines
        .take_while(|line| !line.starts_with('}'))
        .find_map(|line| line.trim_start().strip_prefix("linux /vmlinuz "))
        .map(words)
}

/// Boot the fake machine as GRUB would: its default entry. The kernel's command line and a new
/// boot id are what the next command reads.
fn boot(case: &Case, distro: &str, count: u32) -> Vec<String> {
    let cmdline = next_boot(case, distro);
    case.root
        .write("proc/cmdline", &format!("{}\n", cmdline.join(" ")));
    case.root.write(
        "proc/sys/kernel/random/boot_id",
        &format!("00000000-0000-4000-8000-{count:012}\n"),
    );
    cmdline
}

fn has(words: &[String], word: &str) -> bool {
    words.iter().any(|w| w == word)
}

const SD_VENDOR: &str = "4a67b082-0a4c-41cf-b6c7-440b29bb8c4f";

/// An EFI variable's value, past its four bytes of attributes.
fn efi_var(case: &Case, name: &str) -> Option<Vec<u8>> {
    let path = case.root.path(&format!("{EFIVARS}/{name}"));
    std::fs::read(path).ok().map(|data| data[4..].to_vec())
}

/// The entry the firmware starts once, next, if any.
fn boot_next(case: &Case) -> Option<u16> {
    efi_var(case, &format!("BootNext-{EFI_GLOBAL}")).map(|d| u16::from_le_bytes([d[0], d[1]]))
}

/// Nothing boots once behind the user's back: the last command left no trial entry, GRUB trial
/// script, flag, trial record, `BootNext` or systemd-boot one-shot, and ran nothing that sets
/// one. Every test of a boot change calls it after each apply.
fn no_trial(case: &Case, distro: &str) {
    let esp = esp(distro);
    for path in [
        "etc/grub.d/43_lodi_trial".to_string(),
        "var/lib/lodi/host/boot-trial".to_string(),
        "var/lib/lodi/host/boot-trial-loader".to_string(),
        format!("{esp}/EFI/lodi.env"),
        format!("{esp}/loader/entries/lodi-trial.conf"),
    ] {
        assert!(!case.root.exists(&path), "{distro}: {path}");
    }
    assert_eq!(boot_next(case), None, "{distro}");
    let one_shot = format!("LoaderEntryOneShot-{SD_VENDOR}");
    assert_eq!(efi_var(case, &one_shot), None, "{distro}");
    if case.root.exists(grub_cfg(distro)) {
        let cfg = case.root.read(grub_cfg(distro));
        assert!(!cfg.contains("lodi-trial"), "{distro}: {cfg}");
        assert!(!cfg.contains("lodi.trial"), "{distro}: {cfg}");
    }
    let set_once = ["--bootnext", "set-oneshot", "grub-reboot", "grub2-reboot"];
    for line in case.log() {
        assert!(
            !set_once.iter().any(|word| line.contains(word)),
            "{distro}: {line}"
        );
    }
}

/// A booted machine that declares `parameters`, applied once.
fn declared(case: &Case, distro: &str, parameters: &str) {
    case.set_manifest(&manifest(
        distro,
        &format!("[kernel]\nparameters = [{parameters}]\n"),
    ));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{distro}: {}", story(&apply));
    no_trial(case, distro);
}

#[test]
fn a_parameter_change_is_the_default_at_once() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-at-once-{distro}"), distro, machine);
        let default = boot(&case, distro, 1);
        case.set_manifest(&manifest(
            distro,
            "[kernel]\nparameters = [\"mitigations=off\"]\n",
        ));
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        let line = "+ parameters mitigations=off (the default from the next boot; ";
        assert!(text.contains(line), "{distro}: {text}");
        assert!(text.contains("lodi-known-good"), "{distro}: {text}");
        assert!(!text.contains("confirm"), "{distro}: {text}");
        assert!(changes(&case).is_empty(), "{distro}: {:?}", case.log());
        assert_eq!(case.root.read("etc/default/grub"), GRUB, "{distro}");

        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert!(
            !err(&apply).contains("confirm"),
            "{distro}: {}",
            story(&apply)
        );
        no_trial(&case, distro);
        // The loader's own calls: grubby for the machine's entries, then lodi's on Fedora.
        let calls: Vec<String> = changes(&case)
            .iter()
            .map(|line| line.split(' ').next().unwrap_or("").to_string())
            .collect();
        let want: &[&str] = if distro == "fedora" {
            &["grubby", "grub2-mkconfig"]
        } else {
            &["grub-mkconfig"]
        };
        assert_eq!(calls, want, "{distro}: {:?}", case.log());
        // The next boot has it; the entry that was the default stays as the fallback.
        let next = next_boot(&case, distro);
        assert!(has(&next, "mitigations=off"), "{distro}: {next:?}");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(default.clone()),
            "{distro}"
        );
        assert!(has(&boot(&case, distro, 2), "mitigations=off"), "{distro}");
        assert!(has(&boot(&case, distro, 3), "mitigations=off"), "{distro}");

        // Applied again: nothing to do to the boot.
        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
        assert!(nothing(&again), "{distro}");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(default),
            "{distro}"
        );
    }
}

#[test]
fn the_fallback_becomes_the_entry_that_booted() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-fallback-{distro}"), distro, machine);
        let distribution = boot(&case, distro, 1);
        declared(&case, distro, "\"lodi.test=1\"");
        let first = boot(&case, distro, 2);
        assert!(has(&first, "lodi.test=1"), "{distro}: {first:?}");

        // Changed after a reboot: the entry that booted is the fallback now.
        declared(&case, distro, "\"lodi.test=2\"");
        let next = next_boot(&case, distro);
        assert!(has(&next, "lodi.test=2"), "{distro}: {next:?}");
        assert!(!has(&next, "lodi.test=1"), "{distro}: {next:?}");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(first.clone()),
            "{distro}"
        );

        // Changed again before a reboot: the default has not booted, so the fallback stays the
        // entry that has (LD-492).
        declared(&case, distro, "\"lodi.test=3\"");
        assert!(has(&next_boot(&case, distro), "lodi.test=3"), "{distro}");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(first),
            "{distro}"
        );
        let third = boot(&case, distro, 3);
        declared(&case, distro, "\"lodi.test=4\"");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(third),
            "{distro}"
        );
        assert_ne!(next_boot(&case, distro), distribution, "{distro}");
    }
}

#[test]
fn removal_restores_defaults_keeps_known_good() {
    for (distro, machine) in machines() {
        let case = case(&format!("boot-removal-{distro}"), distro, machine);
        let distribution = boot(&case, distro, 1);
        declared(&case, distro, "\"mitigations=off\", \"lodi.test=1\"");
        let booted = boot(&case, distro, 2);

        case.set_manifest(&manifest(distro, ""));
        let plan = case.plan();
        assert!(
            err(&plan).contains("- parameters mitigations=off lodi.test=1 ("),
            "{distro}: {}",
            story(&plan)
        );
        assert!(err(&plan).contains("lodi-known-good"), "{distro}");
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        no_trial(&case, distro);
        // The distribution's own command line at once, the entry before it as the fallback.
        assert_eq!(case.root.read("etc/default/grub"), GRUB, "{distro}");
        assert_eq!(next_boot(&case, distro), distribution, "{distro}");
        assert_eq!(
            entry(&case, distro, "lodi-known-good"),
            Some(booted),
            "{distro}"
        );
        assert_eq!(boot(&case, distro, 3), distribution, "{distro}");
        let again = case.apply(&[]);
        assert!(again.status.success(), "{distro}: {}", story(&again));
        assert!(changes(&case).is_empty(), "{distro}: {}", story(&again));
        // Fedora's own entry is as the image had it, word for word: only grubby edits it.
        if distro == "fedora" {
            assert_eq!(
                case.root.read("boot/loader/entries/fake.conf"),
                "title Fedora\nlinux /vmlinuz\noptions root=/dev/vda1 console=ttyS0 quiet\n"
            );
        }
    }
}

/// systemd-boot's entries on the ESP, by file name, with their `options`.
fn sd_entries(case: &Case, esp: &str) -> BTreeMap<String, Vec<String>> {
    esp_files(case, esp)
        .into_iter()
        .filter_map(|(path, bytes)| {
            let name = path.strip_prefix("loader/entries/")?.to_string();
            let text = String::from_utf8(bytes).unwrap();
            let options = text
                .lines()
                .find_map(|line| line.strip_prefix("options "))
                .unwrap_or_default();
            Some((
                name,
                options.split_whitespace().map(str::to_string).collect(),
            ))
        })
        .collect()
}

/// Start the fake machine as its firmware would: `BootNext` once, else the first of `BootOrder`.
/// systemd-boot boots its one-shot entry once, else its newest entry that is not lodi's; GRUB
/// boots as [`boot`] does. Returns the file started and the command line.
fn power_on(case: &Case, distro: &str, esp: &str) -> (String, Vec<String>) {
    let count = case
        .root
        .read("proc/sys/kernel/random/boot_id")
        .trim()
        .rsplit('-')
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    let next = boot_next(case);
    let (order, entries) = nvram(case);
    let started = next.unwrap_or(order[0]);
    let dir = case.root.path(EFIVARS);
    let _ = std::fs::remove_file(dir.join(format!("BootNext-{EFI_GLOBAL}")));
    let current: Vec<u8> = 7u32
        .to_le_bytes()
        .into_iter()
        .chain(started.to_le_bytes())
        .collect();
    std::fs::write(dir.join(format!("BootCurrent-{EFI_GLOBAL}")), current).unwrap();
    let file = entries[&started].1.clone();
    if file != SD_BOOT_PATH {
        return (file, boot(case, distro, count));
    }
    let listed = sd_entries(case, esp);
    let one_shot = dir.join(format!("LoaderEntryOneShot-{SD_VENDOR}"));
    let id = match std::fs::read(&one_shot) {
        Ok(data) => {
            std::fs::remove_file(&one_shot).unwrap();
            let words: Vec<u16> = data[4..]
                .chunks_exact(2)
                .map(|p| u16::from_le_bytes([p[0], p[1]]))
                .take_while(|w| *w != 0)
                .collect();
            String::from_utf16(&words).unwrap()
        }
        Err(_) => listed
            .keys()
            .rfind(|name| !name.starts_with("lodi-"))
            .expect("an entry")
            .clone(),
    };
    let cmdline = listed[&id].clone();
    case.root
        .write("proc/cmdline", &format!("{}\n", cmdline.join(" ")));
    case.root.write(
        "proc/sys/kernel/random/boot_id",
        &format!("00000000-0000-4000-8000-{count:012}\n"),
    );
    (file, cmdline)
}

/// A UEFI root that booted once, as a fresh guest has.
fn booted(name: &str, distro: &str, machine: Machine) -> (Case, &'static str) {
    let (case, esp) = uefi(name, distro, machine);
    power_on(&case, distro, esp);
    (case, esp)
}

#[test]
fn a_loader_switch_is_the_default_at_once() {
    for (distro, machine) in machines() {
        let (case, esp) = booted(&format!("switch-at-once-{distro}"), distro, machine);
        case.set_manifest(&manifest(distro, SYSTEMD_BOOT));
        let plan = case.plan();
        assert!(plan.status.success(), "{distro}: {}", story(&plan));
        let text = err(&plan);
        assert!(text.contains("~ loader grub -> systemd-boot ("), "{text}");
        assert!(text.contains("the default from the next boot"), "{text}");
        assert!(!text.contains("confirm"), "{distro}: {text}");
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        assert!(
            !err(&apply).contains("confirm"),
            "{distro}: {}",
            story(&apply)
        );
        no_trial(&case, distro);
        // The new loader first, the earlier one right after it; the firmware starts it next.
        assert_eq!(
            boots(&case, 0),
            SD_BOOT_PATH,
            "{distro}: {:?}",
            nvram(&case)
        );
        assert_eq!(boots(&case, 1), grub_path(distro), "{distro}");
        let (file, cmdline) = power_on(&case, distro, esp);
        assert_eq!(file, SD_BOOT_PATH, "{distro}");
        assert!(has(&cmdline, "lodi.test=1"), "{distro}: {cmdline:?}");
        assert_eq!(power_on(&case, distro, esp).0, SD_BOOT_PATH, "{distro}");
        let settled = case.apply(&[]);
        assert!(settled.status.success(), "{distro}: {}", story(&settled));
        assert!(
            boot_changes(&case).is_empty(),
            "{distro}: {}",
            story(&settled)
        );
        assert!(nothing(&settled), "{distro}: {}", story(&settled));
    }
}

#[test]
fn systemd_boot_parameters_take_effect_at_once() {
    for (distro, machine) in machines() {
        let (case, esp) = booted(&format!("sd-at-once-{distro}"), distro, machine);
        to_systemd_boot(&case, distro);
        let first = power_on(&case, distro, esp).1;
        assert!(has(&first, "lodi.test=1"), "{distro}: {first:?}");
        let two = SYSTEMD_BOOT.replace("lodi.test=1", "lodi.test=2");
        case.set_manifest(&manifest(distro, &two));
        let plan = case.plan();
        let text = err(&plan);
        assert!(text.contains("the default from the next boot"), "{text}");
        assert!(text.contains("lodi-known-good.conf"), "{text}");
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        no_trial(&case, distro);
        let cmdline = case.root.read("etc/kernel/cmdline");
        assert!(cmdline.contains("lodi.test=2"), "{distro}: {cmdline}");
        assert!(!cmdline.contains("lodi.test=1"), "{distro}: {cmdline}");
        assert!(
            case.log()
                .iter()
                .any(|l| l.starts_with("kernel-install ") && l.contains(" add ")),
            "{distro}: {:?}",
            case.log()
        );
        let entries = sd_entries(&case, esp);
        assert_eq!(entries["lodi-known-good.conf"], first, "{distro}");
        let now = power_on(&case, distro, esp).1;
        assert!(has(&now, "lodi.test=2"), "{distro}: {now:?}");
        let settled = case.apply(&[]);
        assert!(nothing(&settled), "{distro}: {}", story(&settled));

        // Changed twice before a reboot: the fallback stays the entry that booted (LD-492).
        for n in [3, 4] {
            let next = SYSTEMD_BOOT.replace("lodi.test=1", &format!("lodi.test={n}"));
            case.set_manifest(&manifest(distro, &next));
            let apply = case.apply(&[]);
            assert!(apply.status.success(), "{distro}: {}", story(&apply));
            no_trial(&case, distro);
            assert_eq!(
                sd_entries(&case, esp)["lodi-known-good.conf"],
                now,
                "{distro}"
            );
        }
        let four = power_on(&case, distro, esp).1;
        assert!(has(&four, "lodi.test=4"), "{distro}: {four:?}");
        assert!(!has(&four, "lodi.test=3"), "{distro}: {four:?}");

        // Taking the parameters out: the machine's own line at once, the entry before it kept.
        let none = SYSTEMD_BOOT.replace("[kernel]\nparameters = [\"lodi.test=1\"]\n\n", "");
        case.set_manifest(&manifest(distro, &none));
        let apply = case.apply(&[]);
        assert!(apply.status.success(), "{distro}: {}", story(&apply));
        no_trial(&case, distro);
        assert_eq!(
            sd_entries(&case, esp)["lodi-known-good.conf"],
            four,
            "{distro}"
        );
        let own = power_on(&case, distro, esp).1;
        assert!(
            !own.iter().any(|w| w.starts_with("lodi.test")),
            "{distro}: {own:?}"
        );
    }
}
