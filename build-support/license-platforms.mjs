// Product targets and npm's published os/cpu selectors share this mapping.
export const productPlatforms = [
  { name: 'macos-arm64', rust: 'aarch64-apple-darwin', os: 'darwin', cpu: 'arm64' },
  { name: 'windows-x64', rust: 'x86_64-pc-windows-msvc', os: 'win32', cpu: 'x64' },
  { name: 'linux-x64', rust: 'x86_64-unknown-linux-gnu', os: 'linux', cpu: 'x64' },
];
export function npmPlatforms(pkg) {
  const matches = (list, value) => !list || (!list.includes('!' + value) && (!list.some(x => !x.startsWith('!')) || list.includes(value)));
  return productPlatforms.filter(p => matches(pkg.os, p.os) && matches(pkg.cpu, p.cpu)).map(p => p.name);
}
