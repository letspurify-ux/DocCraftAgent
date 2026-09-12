package service
import "errors"
func HandleRequest(name string) (string, error) {
    if name == "" { return "", errors.New("name_required") }
    return "Hello, " + name, nil
}
