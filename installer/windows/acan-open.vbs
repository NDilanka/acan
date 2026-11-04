' Opens Acan in the browser from the Start menu or desktop shortcut.
' wscript runs this file with no window, and acan.exe starts with a hidden window, so no console
' appears. acan open starts the server, or reuses one already running, and opens the browser.
' The installer passes --no-browser after a silent update, so the tab that is already open
' reconnects and no second tab appears.
' This script waits for acan open, and on a failure shows where the details are. The log holds
' no token, and neither does the message.
Option Explicit
Dim fso, shell, dir, extra, code, logPath
Set fso = CreateObject("Scripting.FileSystemObject")
Set shell = CreateObject("WScript.Shell")
dir = fso.GetParentFolderName(WScript.ScriptFullName)
shell.CurrentDirectory = dir
extra = ""
If WScript.Arguments.Count > 0 Then
  If WScript.Arguments(0) = "--no-browser" Then extra = " --no-browser"
End If
On Error Resume Next
code = shell.Run("""" & dir & "\acan.exe"" open" & extra, 0, True)
If Err.Number <> 0 Then code = Err.Number
On Error GoTo 0
If code <> 0 Then
  logPath = shell.ExpandEnvironmentStrings("%LOCALAPPDATA%") & "\Acan\open.log"
  MsgBox "Acan could not start. The details are in " & logPath, vbExclamation, "Acan"
End If
