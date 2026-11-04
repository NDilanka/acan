' Opens Acan in the browser from the Start menu or desktop shortcut.
' wscript runs this file with no window, and acan.exe starts with a hidden window, so no console
' appears. acan open starts the server on 127.0.0.1:7311, or reuses one already running there,
' and opens the browser.
Option Explicit
Dim fso, shell, dir
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")
dir = fso.GetParentFolderName(WScript.ScriptFullName)
shell.CurrentDirectory = dir
shell.Run """" & dir & "\acan.exe"" open", 0, False
