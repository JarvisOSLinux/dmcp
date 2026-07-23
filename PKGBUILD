# Maintainer: Yakup Atahanov <yakup.atahanow.b@gmail.com>
pkgname=dmcp
pkgver=0.1.0
pkgrel=1
pkgdesc='MCP Manager - discover, manage, and invoke MCP servers'
url='https://github.com/JarvisOSLinux/dmcp'
license=('GPL-3.0-only')
arch=('x86_64' 'armv6h' 'armv7h' 'aarch64')
makedepends=('cargo')
depends=('gcc-libs')
optdepends=('polkit: for system-scope operations (pkexec)')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
  cd "$pkgname-$pkgver"
  cargo build --release 
}

check() {
  cd "$pkgname-$pkgver"
  cargo test --release 
}

package() {
  # set -x
  cd "$pkgname-$pkgver"
  install -Dm755 target/release/dmcp "$pkgdir/usr/bin/dmcp"
  install -Dm644 policy/org.jarvisos.dmcp.policy "$pkgdir/usr/share/polkit-1/actions/org.jarvisos.dmcp.policy"
  [[ -f man/dmcp.1 ]] && install -Dm644 man/dmcp.1 "$pkgdir/usr/share/man/man1/dmcp.1" || true
}
