# Leaves

A text-mode disk usage visualization utility

## Description

Leaves is a disk usage analyzer inspired by WinDirStat and QDirStat.

It shows files and directories in a hierarchy of nested rectangles.
The area of a rectangle is proportional to its size. A 200 MB file will have twice the size as a sibling with 100 MB. The parent directory will have about 3 times the area of the smaller file.

However, due to the limited resolution of working at a character level, this is a fairly coarse approximation compared to a graphical tool. On the other hand, this will work over remote shell connections when graphical desktop environments are not available or impractical.


## Getting Started

At the time of writing, binary builds are not available yet.

### Dependencies

A Rust toolchain is required to build this project.

#### Option 1: rustup

The standard way to install Rust is via the [rustup script](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

You will likely need to install curl and basic build tools like gcc, make, ld, etc. via the build-essentials/base-devel metapackage or your distribution's equivalent.

#### Option 2: nix

[Nix](https://nixos.org/download/) is an independent package manager and build tool focusing on repeatable, declarative builds.

You can install nix with:

```bash
curl --proto '=https' --tlsv1.2 -L https://nixos.org/nix/install | sh -s -- --daemon
```

Then, to load the build environment from this project directory use nix-shell:

```bash
$ cd leaves
$ nix-shell
```

### Build/Install

Use the cargo build tool to compile the application:

```bash
$ cargo build --release
```

Cargo can also install the application for your current user:

```bash
$ cargo install --path .
```

However, you will need to ensure your environment is configured to
discover executables under the installation destination.

### Executing

To run from source without installing use cargo:

```bash
$ cargo run --release -- [option flags] DIR_TO_SCAN
```

To invoke an installed copy of *leaves* with the target directory:

```bash
leaves ~/Documents
```

Without a target path *leaves* will scan the current directory.

By default, hidden directories will be ignored. Additionally, anything matching patterns in `.gitignore`
or `.ignore` files in the hierarchy will not be counted. You can disable all ignore rules with `-A`. Other options will selectively disable specific ignore sources.

Additionally, you can pass globs (or negations prefixed with `!`) to refine the selection.


## Usage

```console
$ leaves --help
Usage: leaves [OPTIONS] [PATH] [OVERRIDES]...

Arguments:
  [PATH]
          Scanning root path
          
          [default: .]

  [OVERRIDES]...
          Git-style override globs. '!' prefix negates glob

Options:
  -d, --max-depth <MAX_DEPTH>
          Maximum depth of tree to keep in memory.
          
          Subtrees below this depth are replaced with summary nodes. Does not affect scan depth.
          
          [default: 5]

  -x, --xray
          Group files by type at the top-level, then split each region by directory

  -A, --include-all
          Don't *automatically* skip any files. Only overrides will be used

  -H, --include-hidden
          Don't skip hidden files and folders

  -I, --include-ignored
          Don't skip .ignore'd files

  -G, --include-gitignored
          Don't skip .gitignore'd files and folders

  -E, --include-gitexcluded
          Don't skip files and folders listed in .git/info/exclude

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

### Layout

After *leaves* scans the target directory it will enter the main view.

The title bar contains the path of the current view, along with its total size and file count.

On the bottom are shortcut keys for altering the treemap.

The sidebar contains a standard collapsible explorer tree.
File/directory details will be displayed in the box underneath the tree.
Up/down/left/right keys will navigate this tree. The space key will open/close the selected directory.
You can also use the mouse to interact with the explorer.

The central panel shows the files and directories as rectangles.
The area of each rectangle is roughly proportional to the disk size.
However, because each border takes two characters wide/tall at minimum,
deeply nested items will appear smaller than similar top-level items.

Files are colored by extension with a yellow/orange/brown scheme.
Directories are colored by name with a cooler scheme (viridis) comprised primarily of blues and greens.
The palettes can be swapped with the `LEAVES_COLORS=swap` environment variable.

> [!note]
> Both palettes overlap on yellow, but directory colors tend towards more vibrant shades.

Coloring directories by name allows you to visually compare subdirectories in a hierarchy.
For example, you can easily differentiate between debug vs. release or src vs. test directories in different projects.

Similarly, coloring files by extension lets you quickly spot .lock or .json files across directories, for instance.

You can also use the mouse to select files and directories in the explorer. The selected item and its ancestors will be highlighted by thicker borders. The selection is synchronized with the explorer tree, bidirectionally.

### Focus view

Using the Enter key will focus on the selected directory, replacing the contents of the explorer and treemap. Use the Backspace key to go to the parent directory. The previously viewed directory is selected whe navigating back up the hierarchy.

You will not be able to navigate beyond the initial target directory the application was launched with.
To view files outside the current hierarchy, quit the app and restart with a new target.

### Expansion/Deflation

With the limitaions of working with block characters, it is usually not helpful to view millions of files across thousands of directories. This can be visually overwhelming. With each file represented by a handful of characters, little relevant information can be conveyed.

*leaves* will summarize directories below a certain depth, by grouping files of the same extension into a single rectangle with an area propertional to their cumulative size. You can control this depth during launch with `--max-depth`.

During run time, the selected directory can be **e**xpanded or **d**eflated on demand. Deflating a directory will replace its child rectangles with file type summaries.

Expanding a directory will rescan the contents of that directory and more detailed children up to the run time depth. Beyond that depth, files will be grouped into summary nodes, yet again.

The run time depth can be set with the `+` and `-` keys.

### Modes

In addition to partitioning the top level of the view by directory, using *x-ray* mode will show groups for each file type. Each group will then be divided into directories.

You can enter *x-ray* mode from any view with the `x` key.
In x-ray mode you can focus any group or subdirectory, changing the view with Enter and Backspace as before.
To return to normal mode, press the `x` key again.

> [!important]
> you will not be able to navigate to the parent of the x-rayed directory without returning to normal mode.

### Practical Considerations

While *leaves* prunes directories at a relatively shallow depth, you can override it to load every file into memory. Files too small to display with rectangles will be represented by dots. The interface can handle millions of files while being generally responsive. However, there will be some delay when switching modes.

#### Root

When scanning the root `/` directory, be sure to exclude virtual file systems like `/dev`, `/proc`, etc. otherwise you may end up with nonsensical results. Either place an `.ignore` file in the root,
update `~/.config/git/ignore` or use negative overrides:

```bash
leaves -A -d 3 / '!/proc' '!/tmp' '!/run' '!/sys' '!/dev' '!/mnt' '!/nix'
```

#### Links

*leaves* will not follow symbolic links since they don't incur additional disk usage inside the directory.

No attempt is made to detect hard links, so they will be double counted.
