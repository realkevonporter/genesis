project('sevyn-compositor', 'c',
  version: '0.1.0',
  license: 'GPL-3.0-or-later',
  default_options: ['c_std=c11', 'warning_level=2'])

# Stage 2 (the minimal nested wlroots compositor, COMPOSITOR_PLAN.md) joins
# this project: the server sources land in src/ and become a second target
# here. At seed time the only target is the M1 test client, so the build
# stays provable before any server code exists.
wayland_client = dependency('wayland-client')
wayland_scanner = find_program('wayland-scanner')
wayland_protocols = dependency('wayland-protocols')
protocols_dir = wayland_protocols.get_variable('pkgdatadir')

xdg_shell_xml = protocols_dir / 'stable/xdg-shell/xdg-shell.xml'

xdg_shell_client_header = custom_target('xdg-shell-client-header',
  output: 'xdg-shell-client-protocol.h',
  input: xdg_shell_xml,
  command: [wayland_scanner, 'client-header', '@INPUT@', '@OUTPUT@'])

xdg_shell_protocol_c = custom_target('xdg-shell-protocol',
  output: 'xdg-shell-protocol.c',
  input: xdg_shell_xml,
  command: [wayland_scanner, 'private-code', '@INPUT@', '@OUTPUT@'])

executable('sevyn-test-client',
  'tests/test-client.c',
  xdg_shell_client_header,
  xdg_shell_protocol_c,
  dependencies: wayland_client,
  install: false)
