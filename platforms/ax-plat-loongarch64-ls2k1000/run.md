```bash
cargo fmt --package ax-plat-loongarch64-ls2k1000
cargo xtask clippy --package ax-plat-loongarch64-ls2k1000

FEATURES=ax-hal/loongarch64-ls2k1000 \
cargo xtask arceos build \
  --package ax-helloworld \
  --target loongarch64-unknown-none-softfloat
```

On the current LS2K1000 U-Boot, `bootelf -p` loads the ELF program headers but
returns to the U-Boot prompt. Start the loaded entry explicitly with `go`.

```bash
tftpboot ${loadaddr} ax-helloworld
bootelf -p ${loadaddr}
go 0x9000000000200040
```
