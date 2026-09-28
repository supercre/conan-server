from conan import ConanFile
from conan.tools.cmake import CMake, cmake_layout


class Hello(ConanFile):
    name = "forge-hello"
    version = "0.1"
    license = "MIT"
    package_type = "static-library"
    settings = "os", "arch", "compiler", "build_type"
    exports = "build-note.txt"
    exports_sources = "CMakeLists.txt", "src/*"
    generators = "CMakeToolchain"

    def layout(self):
        cmake_layout(self)

    def build(self):
        cmake = CMake(self)
        cmake.configure()
        cmake.build()

    def package(self):
        CMake(self).install()

    def package_info(self):
        self.cpp_info.libs = ["forge_hello"]
