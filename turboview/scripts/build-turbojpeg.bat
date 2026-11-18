@echo off
rem Build libjpeg-turbo (turbojpeg-static) with SIMD. Uses a manually
rem constructed MSVC environment so vcvars64.bat (which needs reg.exe) is
rem not required.
set "VSROOT=C:\Program Files (x86)\Microsoft Visual Studio\18\BuildTools\VC\Tools\MSVC\14.51.36231"
set "SDKROOT=C:\Program Files (x86)\Windows Kits\10"
set "SDKVER=10.0.26100.0"
set "INCLUDE=%VSROOT%\include;%SDKROOT%\Include\%SDKVER%\ucrt;%SDKROOT%\Include\%SDKVER%\um;%SDKROOT%\Include\%SDKVER%\shared;%SDKROOT%\Include\%SDKVER%\winrt"
set "LIB=%VSROOT%\lib\x64;%SDKROOT%\Lib\%SDKVER%\ucrt\x64;%SDKROOT%\Lib\%SDKVER%\um\x64"
set "PATH=%VSROOT%\bin\Hostx64\x64;%SDKROOT%\bin\%SDKVER%\x64;%SDKROOT%\bin\x64;C:\Users\Ralf\.workbuddy\binaries\python\envs\default\Scripts;C:\Users\Ralf\Downloads\libjpeg-turbo-3.2.0\turboview\third-party\nasm\nasm-2.16.03;%PATH%"
cd /d C:\Users\Ralf\Downloads\libjpeg-turbo-3.2.0
cmake -G Ninja -S . -B build-turbo -DCMAKE_BUILD_TYPE=Release -DWITH_SIMD=ON -DWITH_TURBOJPEG=ON -DENABLE_SHARED=OFF -DENABLE_STATIC=ON -DWITH_CRT_DLL=ON -DWITH_JAVA=OFF -DWITH_FUZZ=OFF -DWITH_TOOLS=OFF -DWITH_TESTS=OFF
if errorlevel 1 goto failed
cmake --build build-turbo --config Release --target turbojpeg-static -- -j4
if errorlevel 1 goto failed
echo TURBOJPEG_BUILD_OK
exit /b 0
:failed
echo TURBOJPEG_BUILD_FAILED
exit /b 1
