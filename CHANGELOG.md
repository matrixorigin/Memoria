# Changelog

All notable changes to this project will be documented in this file.

## [0.2.2] - 2026-03-20

### 🐛 Bug Fixes

- Security and snapshot count  (#61) ([eafed50](https://github.com/matrixorigin/Memoria/commit/eafed5001036ff6720281eea5a625b793bcfc036))
- Flaky ci test (#58) ([c788dc0](https://github.com/matrixorigin/Memoria/commit/c788dc072c3d6541d438d75a35fd8fa1d88a7887))

### 🚀 Features

- Feedback system, adaptive retrieval, governance audit trail, API hardening, and comprehensive e2e tests (#65) ([539fd8a](https://github.com/matrixorigin/Memoria/commit/539fd8a3aa33c8503400f8f824af697f1c59ac53))
- Add --tool flag to mcp subcommand and fix codex support (#64) ([1701094](https://github.com/matrixorigin/Memoria/commit/1701094a99fa7677d9e2264d1b92a3be6c67db1b))
- Interactive init prefill, Codex support, self-update, install auto-init (#63) ([6394d8d](https://github.com/matrixorigin/Memoria/commit/6394d8d5b5924a36adf8f5259bfaa436018ef3f2))
- Enable apikey authentication (#47) ([18c6c17](https://github.com/matrixorigin/Memoria/commit/18c6c17f1029244fd30149159a0c8e19142e57dd))
## [0.2.1] - 2026-03-19

### 🏗️ Build

- **ci**: Switch Linux release binaries to musl static linking (#56) ([b651cd1](https://github.com/matrixorigin/Memoria/commit/b651cd1abed188bca6b701e12be17000d8a35183))

### 📚 Documentation

- **openclaw**: Simplify install path and clarify success checks (#57) ([1e5a5f3](https://github.com/matrixorigin/Memoria/commit/1e5a5f3d350ccb26af3362da11533e97ea61823e))

### 🚀 Features

- More skills (#55) ([f62d777](https://github.com/matrixorigin/Memoria/commit/f62d777586e201956f5a86a89b15a476a0873561))

### 🧪 Testing

- Add session consistency test (#54) ([27bf9f8](https://github.com/matrixorigin/Memoria/commit/27bf9f89f5ac52eb6056c72fa0f30754a719cd9d))
## [0.2.0-rc] - 2026-03-19

### 🐛 Bug Fixes

- Update README logo to new memoria-logo asset (#52) ([7d1245f](https://github.com/matrixorigin/Memoria/commit/7d1245faa3e5b867016c91e0b3dd1363df8e8187))
- UTF-8 string truncation panic with multi-byte characters (#48) ([f774499](https://github.com/matrixorigin/Memoria/commit/f774499caa6c6b981d0a8eaa1b4f3d46d831e3ae))
- Install.sh Text file busy error when upgrading (#46) ([a79f971](https://github.com/matrixorigin/Memoria/commit/a79f97120b33ce1f91aef42518073fdc806be472))

### 🚀 Features

- **benchmark**: Separate official LongMemEval and BEAM reporting (#50) ([72998fe](https://github.com/matrixorigin/Memoria/commit/72998fe38676209c6a75de2da606998d8da26414))
- Add OpenClaw-native Memoria onboarding (#49) ([7560190](https://github.com/matrixorigin/Memoria/commit/75601906907079417f6756b86ee538d8085df14e))
- Implement plugin framework (#45) ([4301f97](https://github.com/matrixorigin/Memoria/commit/4301f976e0757b9d465b43891bd20745aea17a28))
- Replace hand-rolled prompts with cliclack TUI wizard (#44) ([ef071bd](https://github.com/matrixorigin/Memoria/commit/ef071bd33be262154fee23903edaa33fbe81d4b6))
## [0.1.0] - 2026-03-18

### 🐛 Bug Fixes

- Install.sh latest URL format (/releases/latest/download/ not /releases/download/latest/) ([ddbd06e](https://github.com/matrixorigin/Memoria/commit/ddbd06edc9885257ea2b70fa24d67db076e92351))
- Docker release username (#43) ([18ebca5](https://github.com/matrixorigin/Memoria/commit/18ebca503f4017ea81cd152b83e32afea17ec01d))

### 🚀 Features

- **cli**: Connectivity checks for DB and embedding in interactive init (#42) ([aedb529](https://github.com/matrixorigin/Memoria/commit/aedb52998949cfefb6cf6375130001f34da1dd71))
- Interactive setup wizard (memoria init -i) and improved install script (#41) ([e0df7b9](https://github.com/matrixorigin/Memoria/commit/e0df7b9959863a3fea2bff7f76063f5f89e9a02e))
## [0.1.0-rc2] - 2026-03-17

### 🐛 Bug Fixes

- Missing from rust refactor  (#39) ([fb38d6c](https://github.com/matrixorigin/Memoria/commit/fb38d6c535a363368a6fb17ac0ead7c9da65f8e8))
## [0.1.0-rc1] - 2026-03-17

### Sync

- Update to v0.2.5 - auto table creation, embedding dim switching ([0379674](https://github.com/matrixorigin/Memoria/commit/037967407571a76c5f5809af7583dcff49b392e8))

### ⚡ Performance

- Small opts (#27) ([2e9af19](https://github.com/matrixorigin/Memoria/commit/2e9af196d0cdefe6718b6f8d245cc9d5137ec734))
- Enhance explain (#26) ([cf60fce](https://github.com/matrixorigin/Memoria/commit/cf60fcec9c4447c636b4b1adc3d6eb6488436cb4))
- Improve memory retrieval and graph operations (#21) ([aa50616](https://github.com/matrixorigin/Memoria/commit/aa50616d7d31277f0b610e27abd656a84ec58fba))

### 🐛 Bug Fixes

- Cross toml ([f7d24ad](https://github.com/matrixorigin/Memoria/commit/f7d24ad0d725070ebf6298c54c200464a8cd6d7b))
- Cross compiling env (#38) ([3b35ac1](https://github.com/matrixorigin/Memoria/commit/3b35ac1e6ae732b5bb88ba6e271abe6c7a90f2e7))
- Release dep (#36) ([5c7b347](https://github.com/matrixorigin/Memoria/commit/5c7b3476992a58628191eb2e06140eeceb843c8c))
- Remove hardcoded version in memoria-git dependency (#35) ([5b142e6](https://github.com/matrixorigin/Memoria/commit/5b142e6e796e758734fa337cba9efa2e642b3d3d))
- Ci related (#34) ([e1b7e0c](https://github.com/matrixorigin/Memoria/commit/e1b7e0c911926e66df93ed3e6c6d113ba24906b1))
- Hybrid search (#30) ([e1f319f](https://github.com/matrixorigin/Memoria/commit/e1f319f3985e25c62f7316b06eab7aa9395c9c6f))
- Issue #22. (#23) ([068dda3](https://github.com/matrixorigin/Memoria/commit/068dda373ea13d741770f31ebbb9b661436775f8))
- Batch inject (#24) ([85c4675](https://github.com/matrixorigin/Memoria/commit/85c4675a95c45d4b1f53821ab964f6c6aa7bca98))
- Sync version to 0.1.14 and remove 500-char limit in rule version… (#20) ([ad84469](https://github.com/matrixorigin/Memoria/commit/ad8446936bb67c590c8f23543d9e9fe4cd60e6ed))
- Stale graph node retrieval and cooldown cache pollution (#15) ([b1dd0b3](https://github.com/matrixorigin/Memoria/commit/b1dd0b382e2597d1c8f333c3e9e8d94b82d58287))
- Resolve branch name hyphen bug, add input validation, optimize CI workflows (#8) ([cc82cf9](https://github.com/matrixorigin/Memoria/commit/cc82cf90cd7ddb97311dd4f42a65a534c0d277cb))

### 📚 Documentation

- Add CPU-only PyTorch installation guide for non-GPU environments (#12) ([8e1af3a](https://github.com/matrixorigin/Memoria/commit/8e1af3a6b64d44e9e3c846ad0b4073663388e26d))
- Guide users to edit config files after init if needed ([734f5ed](https://github.com/matrixorigin/Memoria/commit/734f5ed3c3b022d0cb96c41699a64c66e74a8240))
- Emphasize embedding config is irreversible - add critical warnings ([31971e3](https://github.com/matrixorigin/Memoria/commit/31971e3d4e4c31100871d3ec9268749e43160c42))
- Emphasize guided config with all env vars always present ([bee7f7d](https://github.com/matrixorigin/Memoria/commit/bee7f7d1f4934dd51e31ae73779ce5c82a49e4b0))

### 📦 Miscellaneous

- Remove openssl-sys ([598bd66](https://github.com/matrixorigin/Memoria/commit/598bd66bcbc3382a1c9fef1a53450d88ebda0d33))
- Consolidate test commands and update package name to mo-memoria (#7) ([7d94c6c](https://github.com/matrixorigin/Memoria/commit/7d94c6c1bde25c3bc3b4347c624a6c178e4dc52b))
- Add mergify (#4) ([b71f0fe](https://github.com/matrixorigin/Memoria/commit/b71f0fe7c0a5a559152aad4115bd9f30d3fce0d6))

### 🔧 Refactoring

- Refactor all with rust (#33) ([62a7076](https://github.com/matrixorigin/Memoria/commit/62a7076b513925b382db07051d021aba5b6060c4))

### 🚀 Features

- Add installer script and improve release workflow (#37) ([32fae53](https://github.com/matrixorigin/Memoria/commit/32fae53d1ae25eda373d994c3dc51d5737833459))
- Lots of changes to api server and bug fixes (#29) ([1b3d479](https://github.com/matrixorigin/Memoria/commit/1b3d47980522dd7d086937379fefef1852c8ea64))
- Enhance with explain (#25) ([07fed35](https://github.com/matrixorigin/Memoria/commit/07fed357a2a8e7cf4309d6a8fd003c766835e147))
- Optimize MCP server startup and database configuration loading (#16) ([e908548](https://github.com/matrixorigin/Memoria/commit/e90854805ddcde4a1326d2560f6b3d7f582f75a0))
- Enable offline mode for local embedding by default (#3) ([d446a13](https://github.com/matrixorigin/Memoria/commit/d446a1309f44de5b8b9d50a245876baefd88bee1))
- L0/L1 tiered memory retrieval + redundancy compression + governance improvements (#2) ([9d99085](https://github.com/matrixorigin/Memoria/commit/9d9908572cf6d758dafff6d92a8cb8373b97d499))

