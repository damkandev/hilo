# dmgbuild settings for hilo-macos.dmg: a brushed-metal window, hilo on the left, an arrow, Applications
# on the right. bundle.sh passes app= (the built hilo.app), icon= (its .icns) and bg= (assets/dmg-background.tiff) with -D.

app = defines["app"]  # noqa: F821 (dmgbuild provides `defines`)

format = "UDZO"
files = [app]
symlinks = {"Applications": "/Applications"}
icon = defines["icon"]  # noqa: F821 (the volume's icon on the desktop)

# the background is 600×400 (assets/dmg.html); the window adds its title bar
background = defines["bg"]  # noqa: F821
window_rect = ((200, 120), (600, 428))
icon_size = 128
text_size = 13
icon_locations = {"hilo.app": (150, 250), "Applications": (450, 250)}
default_view = "icon-view"
show_status_bar = show_tab_view = show_toolbar = show_pathbar = show_sidebar = False
