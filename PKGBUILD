# Maintainer: Christian Balcom <robot.inventor@gmail.com>

pkgname=prism-notify
pkgver=0.1.0
pkgrel=1
pkgdesc='Notification daemon for the prism compositor — damascene-rendered, layer-shell native'
arch=('x86_64')
url='https://github.com/computer-whisperer/prism-notify'
license=('MIT OR Apache-2.0')
# wayland (libwayland-client) and vulkan-icd-loader are dlopened at
# runtime (wgpu's dlopen path), not linked — ldd shows only glibc and
# gcc-libs. zbus is pure Rust; no libdbus.
depends=(
    'gcc-libs'
    'glibc'
    'vulkan-icd-loader'
    'wayland'
)
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('ce67bc248de46545177eb83b3edd48b9d86fba4b34d81f3db0b0a07a665d7c49')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --release --frozen
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --release --frozen
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 "target/release/prism-notify" "$pkgdir/usr/bin/prism-notify"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    # D-Bus activation: the bus spawns the daemon on the first Notify()
    # call if nothing owns org.freedesktop.Notifications yet. Installed
    # under a unique filename so it can't file-conflict with other
    # daemons' service files (notification-daemon ships the canonical
    # org.freedesktop.Notifications.service name).
    install -Dm644 prism-notify.service \
        "$pkgdir/usr/share/dbus-1/services/prism-notify.service"
    install -Dm644 LICENSE-MIT "$pkgdir/usr/share/licenses/$pkgname/LICENSE-MIT"
    install -Dm644 LICENSE-APACHE "$pkgdir/usr/share/licenses/$pkgname/LICENSE-APACHE"
}
