#include <string>
#include <stdexcept>
std::string handle_request(const std::string& name) {
    if (name.empty()) throw std::invalid_argument("name_required");
    return "Hello, " + name;
}
