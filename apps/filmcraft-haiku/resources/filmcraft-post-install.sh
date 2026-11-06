#!/bin/sh
# Registers FilmCraft's project type and makes FilmCraft the preferred application of it (and of
# the movie types nothing else claimed yet); a preferred application the user chose stays.
signature=application/x-vnd.storytold-FilmCraft
app=$(findpaths -p "$0" B_FIND_PATH_APPS_DIRECTORY FilmCraft 2>/dev/null)
[ -n "$app" ] && [ -e "$app" ] && mimeset -a -f "$app"
setmime -set application/x-filmcraft -short "FilmCraft project" -long "FilmCraft video editing project" -extension fcproj
for type in application/x-filmcraft video/mp4 video/quicktime video/x-matroska video/webm; do
    setmime -dump "$type" 2>/dev/null | grep -q -- -preferredAppSig \
        || setmime -set "$type" -preferredAppSig "$signature"
done
exit 0
