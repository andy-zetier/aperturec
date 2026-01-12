# Changelog

## [2.2.0](https://github.com/Zetier/aperturec/compare/aperturec-server-v2.1.0...aperturec-server-v2.2.0) (2026-01-12)


### Features

* Add InterFrameInterval histogram metric and observe frame gaps ([#768](https://github.com/Zetier/aperturec/issues/768)) ([1a7625c](https://github.com/Zetier/aperturec/commit/1a7625c3b8ee874e370e3309de3306482cbc2523))
* Add metrics for DroppedInvalidKeycode and DroppedEventSendError ([#766](https://github.com/Zetier/aperturec/issues/766)) ([8875377](https://github.com/Zetier/aperturec/commit/887537765a7b24ade3882651f4c99b931c4e1a73))


### Bug Fixes

* Add machete ignores to reduce noise in cargo-machete ([#773](https://github.com/Zetier/aperturec/issues/773)) ([ad42cdc](https://github.com/Zetier/aperturec/commit/ad42cdc09fe3200b79cf4061fd17f56b1ae0b1ea))

## [2.1.0](https://github.com/Zetier/aperturec/compare/aperturec-server-v2.0.3...aperturec-server-v2.1.0) (2025-12-16)


### Features

* Add shared compressor pools for zlib and jpegxl ([#763](https://github.com/Zetier/aperturec/issues/763)) ([995e9a5](https://github.com/Zetier/aperturec/commit/995e9a5ffb7db3079e65a391f068af20404739b6))

## [2.0.3](https://github.com/Zetier/aperturec/compare/aperturec-server-v2.0.2...aperturec-server-v2.0.3) (2025-12-16)

## [2.0.2](https://github.com/Zetier/aperturec/compare/aperturec-server-v2.0.1...aperturec-server-v2.0.2) (2025-12-10)


### Bug Fixes

* Allow TCP streams to drain naturally on client close ([#746](https://github.com/Zetier/aperturec/issues/746)) ([d82b82f](https://github.com/Zetier/aperturec/commit/d82b82f929525bd37b28e7fc97afb03b8238aeb7))
* Close tunnels without draining on client initialization ([#747](https://github.com/Zetier/aperturec/issues/747)) ([eb4200e](https://github.com/Zetier/aperturec/commit/eb4200e337568e67173d2b48c16c0469bd9b98b3))

## [2.0.1](https://github.com/Zetier/aperturec/compare/aperturec-server-v2.0.0...aperturec-server-v2.0.1) (2025-12-03)


### Bug Fixes

* Properly handle failures in accepting new sessions ([#721](https://github.com/Zetier/aperturec/issues/721)) ([5edca36](https://github.com/Zetier/aperturec/commit/5edca366f7856e78da2e4b7ea435ba8ea07e3c75))

## [2.0.0](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.7...aperturec-server-v2.0.0) (2025-10-27)


### ⚠ BREAKING CHANGES

* migrate aperturec-channel to concrete error types ([#667](https://github.com/Zetier/aperturec/issues/667))

### Features

* Add reset_session_state to backend ([#671](https://github.com/Zetier/aperturec/issues/671)) ([3158163](https://github.com/Zetier/aperturec/commit/31581632c5ad508197163545ca1dffab767a78eb))
* Migrate aperturec-channel to concrete error types ([#667](https://github.com/Zetier/aperturec/issues/667)) ([958569b](https://github.com/Zetier/aperturec/commit/958569baa115f7d4aaf483c72944a83a9ac5d5af))


### Bug Fixes

* Remove default impl in process_utils ([#673](https://github.com/Zetier/aperturec/issues/673)) ([c96d6f5](https://github.com/Zetier/aperturec/commit/c96d6f5470ec95f8fd17c01994df57fa7d0adab8))

## [1.1.7](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.6...aperturec-server-v1.1.7) (2025-10-07)


### Bug Fixes

* Run clippy fixes ([#649](https://github.com/Zetier/aperturec/issues/649)) ([9a323e0](https://github.com/Zetier/aperturec/commit/9a323e087f344b26529e97b59ea2ccd3246e0bc3))

## [1.1.6](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.5...aperturec-server-v1.1.6) (2025-09-29)


### Bug Fixes

* Proper directory structure to download artifacts ([8c44441](https://github.com/Zetier/aperturec/commit/8c44441e55ebed8fc7e72245bc64f728b0253644))

## [1.1.5](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.4...aperturec-server-v1.1.5) (2025-09-29)


### Bug Fixes

* Grab all CI artifacts with --paginate ([84a5d3d](https://github.com/Zetier/aperturec/commit/84a5d3d7610597f35ee6547e685687a65eea8803))

## [1.1.4](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.3...aperturec-server-v1.1.4) (2025-09-29)


### Bug Fixes

* Fix package paths in attach & push scripts ([99fde87](https://github.com/Zetier/aperturec/commit/99fde87ead67a9c86d0df238c8417d646dad4d83))

## [1.1.3](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.2...aperturec-server-v1.1.3) (2025-09-26)


### Bug Fixes

* Repair gh api command ([3c8b45d](https://github.com/Zetier/aperturec/commit/3c8b45dd50d05f20b8319bb5aa5193965b3a0ada))

## [1.1.2](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.1...aperturec-server-v1.1.2) (2025-09-26)


### Bug Fixes

* Handle multiple CI runs better and force release ([817b1b2](https://github.com/Zetier/aperturec/commit/817b1b21aa58f06446334311b4dc5a1c21be7b6b))

## [1.1.1](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.1.0...aperturec-server-v1.1.1) (2025-09-26)


### Bug Fixes

* Force releases and bump semver patch ([a45bf7c](https://github.com/Zetier/aperturec/commit/a45bf7c9b74be5a316edcb61fed60fbe07a5ef95))

## [1.1.0](https://github.com/Zetier/aperturec/compare/aperturec-server-v1.0.0...aperturec-server-v1.1.0) (2025-09-26)


### Features

* Set up Copilot instructions with comprehensive build guide and linting fixes ([#597](https://github.com/Zetier/aperturec/issues/597)) ([56e4969](https://github.com/Zetier/aperturec/commit/56e4969e218d12be6c178b42c2088ee317318119))


### Bug Fixes

* Avoid X races during display setup with server grab ([#584](https://github.com/Zetier/aperturec/issues/584)) ([94ec4cf](https://github.com/Zetier/aperturec/commit/94ec4cf713b0647ed0c646c896457263294aa069))
* Observe 100% for full damage in TrackingBuffer ([#602](https://github.com/Zetier/aperturec/issues/602)) ([a8a8634](https://github.com/Zetier/aperturec/commit/a8a8634e45ef71eaf4ac16d8aae5ffe4d58616bb))
* Report full screen damage after resize ([#591](https://github.com/Zetier/aperturec/issues/591)) ([6e01a22](https://github.com/Zetier/aperturec/commit/6e01a22321a34622987be3f7a5243e496243f0d2))

## 1.0.0 (2025-09-17)


### Features

* Initial semver migration + release-please ([#581](https://github.com/Zetier/aperturec/issues/581)) ([c289c65](https://github.com/Zetier/aperturec/commit/c289c650e0ef34948351a92c343137ea5c635c67))
