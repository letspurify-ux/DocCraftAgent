class Service {
    public String handleRequest(String name) {
        if (name.isBlank()) throw new IllegalArgumentException("name_required");
        return "Hello, " + name;
    }
}
