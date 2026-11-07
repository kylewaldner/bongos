# Cli

## Testing

You can test your changes to the `cli` crate by first building the main bongos binary:

```
cargo build -p bongos
```

And then building and running the `cli` crate with the following parameters:

```
 cargo run -p cli -- --editor ./target/debug/bongos.exe
```
