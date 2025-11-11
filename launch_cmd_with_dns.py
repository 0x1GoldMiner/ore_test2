#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
启动带有指定DNS配置的CMD命令提示符
"""

import subprocess
import sys
import platform

def launch_cmd_with_dns(dns_server="8.8.8.8", interface_name=None):
    """
    启动一个CMD窗口并配置使用指定的DNS服务器

    参数:
        dns_server: DNS服务器地址 (默认: 8.8.8.8 - Google DNS)
        interface_name: 网络接口名称 (Windows: 如 "以太网", "Wi-Fi"; Linux: 如 "eth0")
    """

    system = platform.system()

    if system == "Windows":
        # Windows系统
        print(f"正在为Windows系统配置DNS: {dns_server}")

        # 如果没有指定接口，尝试获取默认接口
        if not interface_name:
            interface_name = "以太网"  # 默认接口名，可能需要根据实际情况调整
            print(f"使用默认网络接口: {interface_name}")

        # 创建批处理脚本来设置DNS并启动cmd
        batch_script = f"""@echo off
echo 正在设置DNS服务器为: {dns_server}
netsh interface ip set dns "{interface_name}" static {dns_server}
echo DNS已设置完成
echo 当前DNS配置:
ipconfig /all | findstr /C:"DNS"
echo.
echo 提示: 当你关闭此窗口后，DNS设置将保持不变
echo 如需恢复自动DNS，请运行: netsh interface ip set dns "{interface_name}" dhcp
echo.
cmd /k
"""

        # 写入临时批处理文件
        with open("temp_dns_cmd.bat", "w", encoding="gbk") as f:
            f.write(batch_script)

        print("正在启动CMD窗口...")
        # 以管理员权限启动（需要管理员权限才能修改DNS）
        subprocess.Popen(["cmd", "/c", "temp_dns_cmd.bat"],
                        creationflags=subprocess.CREATE_NEW_CONSOLE)

    elif system == "Linux":
        # Linux系统
        print(f"正在为Linux系统配置DNS: {dns_server}")

        # Linux下修改DNS的脚本
        bash_script = f"""#!/bin/bash
echo "正在设置DNS服务器为: {dns_server}"
echo "nameserver {dns_server}" | sudo tee /etc/resolv.conf
echo "DNS已设置完成"
echo "当前DNS配置:"
cat /etc/resolv.conf
echo ""
echo "提示: 此DNS设置在系统重启或网络重启后可能会恢复"
bash
"""

        # 写入临时脚本
        with open("temp_dns_terminal.sh", "w") as f:
            f.write(bash_script)

        print("正在启动终端窗口...")
        # 尝试使用常见的终端模拟器
        terminals = ["gnome-terminal", "xterm", "konsole", "xfce4-terminal"]
        for term in terminals:
            try:
                subprocess.Popen([term, "--", "bash", "temp_dns_terminal.sh"])
                break
            except FileNotFoundError:
                continue

    else:
        print(f"不支持的操作系统: {system}")
        sys.exit(1)


def main():
    """主函数"""
    print("=" * 60)
    print("CMD/终端 DNS配置工具")
    print("=" * 60)

    # 配置参数
    dns_server = input("请输入DNS服务器地址 (直接回车使用 8.8.8.8): ").strip()
    if not dns_server:
        dns_server = "8.8.8.8"

    interface_name = None
    if platform.system() == "Windows":
        interface_name = input("请输入网络接口名称 (直接回车使用 '以太网'): ").strip()
        if not interface_name:
            interface_name = "以太网"

    print()
    print("常用DNS服务器参考:")
    print("  Google DNS:      8.8.8.8 / 8.8.4.4")
    print("  Cloudflare DNS:  1.1.1.1 / 1.0.0.1")
    print("  阿里DNS:         223.5.5.5 / 223.6.6.6")
    print("  腾讯DNS:         119.29.29.29")
    print()

    print(f"将要设置DNS为: {dns_server}")
    if interface_name:
        print(f"网络接口: {interface_name}")

    confirm = input("确认启动? (y/n): ").strip().lower()
    if confirm == 'y':
        launch_cmd_with_dns(dns_server, interface_name)
        print("完成！")
    else:
        print("已取消")


if __name__ == "__main__":
    main()
