terraform {
  required_version = ">= 1.5"
}

output "greeting" {
  value = var.greeting
}
