# xmip-core-resilience-circuit-breaker

Circuit breaker guard: consecutive failures open the circuit, attempts are refused while it is open, and one trial after the open period decides whether it closes. A technology of [xmip-core-resilience](https://github.com/IlleNilsson/xmip-core-resilience).

## Toolchain

`rust-toolchain.toml` pins the toolchain for the whole estate. Do not change it
here.

## Verification

The included workflow is manual-only and calls the versioned shared workflow at
`IlleNilsson/.github@v1`.
