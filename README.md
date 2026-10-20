# Computer Use

An MCP server that lets agent apps see the Cutefish desktop and use its
pointer and keyboard, once the user allows it. It speaks MCP over stdio and
drives cutefish-compositor's `cutefish_computer_control_v1` and
`ext_image_copy_capture`; the compositor asks the user for consent and can
stop the agent at any time.

## Dependencies

Rust (edition 2021) and the crates in `Cargo.toml`. `.cargo/config.toml`
builds offline against the Debian packages:

```shell
sudo apt install rustc cargo librust-wayland-client-dev librust-wayland-scanner-dev \
  librust-wayland-protocols-dev librust-wayland-backend-dev librust-serde-json-dev \
  librust-png-dev librust-jpeg-encoder-dev librust-base64-dev librust-bitflags-dev \
  librust-libc-dev librust-memmap2-dev
```

## Build

```shell
cargo build --release
```

## Install

```shell
sudo install -m 755 target/release/cutefish-computer-use /usr/bin/
```

The compositor only trusts the server at `/usr/bin/cutefish-computer-use`.

## Use

Add it to an agent app as a stdio MCP server, for example:

```shell
claude mcp add cutefish -- /usr/bin/cutefish-computer-use
```

## License

BSD-3-Clause; see [LICENSE](LICENSE).
