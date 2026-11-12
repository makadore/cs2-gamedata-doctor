@echo off
rem Rebuild the fixture DLLs (MSVC RTTI, x64) from src\*.cpp. The committed old.dll/new.dll are the
rem output of this script; tests do not need a compiler. Run from a Visual Studio x64 developer
rem prompt, or let the script find vcvars64.bat of VS 2022 Community.
setlocal
cd /d "%~dp0"
where cl >nul 2>nul || call "C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat" >nul
for %%v in (old new) do (
  cl /nologo /std:c++17 /O1 /GR /EHsc /MD /LD /Fe:%%v.dll src\%%v.cpp /link /INCREMENTAL:NO /OPT:REF /OPT:ICF || exit /b 1
  del %%v.exp %%v.lib %%v.obj 2>nul
)
dir /b old.dll new.dll
