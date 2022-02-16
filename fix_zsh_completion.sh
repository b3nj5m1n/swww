#!/bin/sh
#
# This is script is necessary because, by default, the output of clap generate
# will give us an autocomplete file that won't suggest any files when you write
# <fswww img>
# and then press 'TAB'.
#
# Bash seems to be fine, and I haven't tested neither fish nor elvish
#

# These are simply the formats supported by the image crate
SUPPORTED_FILES="*.png|*.jpg|*.jpeg|*.gif|*.bmp|*.tif|*.tiff|*.ico|*.webp|*.avif|*.pnm|*.pbm|*.pgm|*.ppm|*.dds|*.tga|*.exr|*.ff|*.farbfeld"

# in order we fix:
# 	img <path>
#	init -i|--img <path>
sed \
	-e "s/:path .*:/&_files -g \"$SUPPORTED_FILES\"/" \
	-e "s/:IMG:/&_files -g \"$SUPPORTED_FILES\"/g" \
	completions/_fswww > completions/tmp \
	&& mv completions/tmp completions/_fswww
