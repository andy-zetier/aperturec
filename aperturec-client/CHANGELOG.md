# Changelog

## [1.2.0](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.1.4...aperturec-client-v1.2.0) (2026-01-14)


### Features

* Split tracing + logs from user output ([#781](https://github.com/Zetier/aperturec/issues/781)) ([e72a85a](https://github.com/Zetier/aperturec/commit/e72a85ae40fe1497ced9e87c3185c1ff6bdb529c))


### Bug Fixes

* Ensure decoder count exceeds display count in client ([#793](https://github.com/Zetier/aperturec/issues/793)) ([04e3ce8](https://github.com/Zetier/aperturec/commit/04e3ce89438a89f26b6921adde609cb59fe2b184))

## [1.1.4](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.1.3...aperturec-client-v1.1.4) (2026-01-12)


### Bug Fixes

* Add machete ignores to reduce noise in cargo-machete ([#773](https://github.com/Zetier/aperturec/issues/773)) ([ad42cdc](https://github.com/Zetier/aperturec/commit/ad42cdc09fe3200b79cf4061fd17f56b1ae0b1ea))

## [1.1.3](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.1.2...aperturec-client-v1.1.3) (2025-12-16)

## [1.1.2](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.1.1...aperturec-client-v1.1.2) (2025-12-10)

## [1.1.1](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.1.0...aperturec-client-v1.1.1) (2025-12-03)


### Bug Fixes

* Close tunnel properly on client close ([#729](https://github.com/Zetier/aperturec/issues/729)) ([8337170](https://github.com/Zetier/aperturec/commit/8337170f8ccadd7505232c039ba6ef75c849ad3f))

## [1.1.0](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.0.1...aperturec-client-v1.1.0) (2025-11-24)


### Features

* Add gtk3 keyboard grab ([#694](https://github.com/Zetier/aperturec/issues/694)) ([b923087](https://github.com/Zetier/aperturec/commit/b9230873bd0f8e861b077bc32b0c18516de29dfc))


### Bug Fixes

* Restore client version logging ([#706](https://github.com/Zetier/aperturec/issues/706)) ([30a4fa5](https://github.com/Zetier/aperturec/commit/30a4fa5beeab0e2f1e99e3cc0de1229c58136f81))

## [1.0.1](https://github.com/Zetier/aperturec/compare/aperturec-client-v1.0.0...aperturec-client-v1.0.1) (2025-11-11)


### Bug Fixes

* Send synthetic caps lock keystroke on macos ([#695](https://github.com/Zetier/aperturec/issues/695)) ([bcc4d62](https://github.com/Zetier/aperturec/commit/bcc4d62855b6ff21668dfe266b4bdb70208674fc))

## [1.0.0](https://github.com/Zetier/aperturec/compare/aperturec-client-v0.3.0...aperturec-client-v1.0.0) (2025-10-27)


### ⚠ BREAKING CHANGES

* change FFI library to have a client connection object ([#678](https://github.com/Zetier/aperturec/issues/678))
* migrate aperturec-channel to concrete error types ([#667](https://github.com/Zetier/aperturec/issues/667))

### Features

* Add client ffi module behind ffi-lib feature ([#661](https://github.com/Zetier/aperturec/issues/661)) ([b9fd592](https://github.com/Zetier/aperturec/commit/b9fd592c4a8f2038f2edb09e1cf53e3b1db06249))
* Migrate aperturec-channel to concrete error types ([#667](https://github.com/Zetier/aperturec/issues/667)) ([958569b](https://github.com/Zetier/aperturec/commit/958569baa115f7d4aaf483c72944a83a9ac5d5af))


### Code Refactoring

* Change FFI library to have a client connection object ([#678](https://github.com/Zetier/aperturec/issues/678)) ([4f954e0](https://github.com/Zetier/aperturec/commit/4f954e0d1c65ea6597d110a950d22e5c6e1dcb4a))

## [0.3.0](https://github.com/Zetier/aperturec/compare/aperturec-client-v0.2.1...aperturec-client-v0.3.0) (2025-10-07)


### Features

* Add iOS OS variant and client mapping ([#645](https://github.com/Zetier/aperturec/issues/645)) ([b297082](https://github.com/Zetier/aperturec/commit/b29708283f59b849727daf456db2fcbe5175a601))


### Bug Fixes

* Prevent client crash on PrtScr key press on Windows ([#643](https://github.com/Zetier/aperturec/issues/643)) ([beba80b](https://github.com/Zetier/aperturec/commit/beba80b01a4a8d3897b58aba80d1e79afbb5f008))
* Run clippy fixes ([#649](https://github.com/Zetier/aperturec/issues/649)) ([9a323e0](https://github.com/Zetier/aperturec/commit/9a323e087f344b26529e97b59ea2ccd3246e0bc3))

## [0.2.1](https://github.com/Zetier/aperturec/compare/aperturec-client-v0.2.0...aperturec-client-v0.2.1) (2025-09-30)


### Bug Fixes

* Prevent clients on macos/windows from trying MM ([#639](https://github.com/Zetier/aperturec/issues/639)) ([cdf53bf](https://github.com/Zetier/aperturec/commit/cdf53bfa690d2326319bdc9c355394053c26d2c1))
* Show modal error on GTK UI creation failure ([#641](https://github.com/Zetier/aperturec/issues/641)) ([c2c55af](https://github.com/Zetier/aperturec/commit/c2c55af7ab328c7cca9f58f92f311e7870a506ee))

## [0.2.0](https://github.com/Zetier/aperturec/compare/aperturec-client-v0.1.0...aperturec-client-v0.2.0) (2025-09-26)


### Features

* Set up Copilot instructions with comprehensive build guide and linting fixes ([#597](https://github.com/Zetier/aperturec/issues/597)) ([56e4969](https://github.com/Zetier/aperturec/commit/56e4969e218d12be6c178b42c2088ee317318119))


### Bug Fixes

* Allow optional shift on Ctrl-Alt-F* keyboard shortcuts ([#606](https://github.com/Zetier/aperturec/issues/606)) ([1618147](https://github.com/Zetier/aperturec/commit/16181479cc18cd252ec8cdb3670e711c5a903012))

## 0.1.0 (2025-09-17)


### Features

* Initial semver migration + release-please ([#581](https://github.com/Zetier/aperturec/issues/581)) ([c289c65](https://github.com/Zetier/aperturec/commit/c289c650e0ef34948351a92c343137ea5c635c67))
