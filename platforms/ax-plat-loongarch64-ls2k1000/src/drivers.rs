mod ahci;

pub(crate) fn init() {
    ahci::probe();
}
