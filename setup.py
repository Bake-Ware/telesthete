"""
Telesthete - Lightweight P2P communication library
"""

from setuptools import setup, find_packages

with open("README.md", "r", encoding="utf-8") as fh:
    long_description = fh.read()

setup(
    name="telesthete",
    version="0.1.0",
    author="Bake-Ware",
    author_email="jamixzol@gmail.com",
    description="Lightweight P2P communication library with encrypted streams",
    long_description=long_description,
    long_description_content_type="text/markdown",
    url="https://github.com/Bake-Ware/telesthete",
    packages=find_packages(),
    classifiers=[
        "Development Status :: 3 - Alpha",
        "Intended Audience :: Developers",
        "Topic :: System :: Networking",
        "License :: OSI Approved :: MIT License",
        "Programming Language :: Python :: 3",
        "Programming Language :: Python :: 3.10",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
    ],
    python_requires=">=3.10",
    install_requires=[
        "PyNaCl>=1.5.0",
    ],
)
