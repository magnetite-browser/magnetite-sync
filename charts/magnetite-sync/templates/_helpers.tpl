{{/* The release's name for everything this chart creates. */}}
{{- define "magnetite-sync.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "magnetite-sync.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "magnetite-sync.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "magnetite-sync.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
A secretKeyRef for a setting given as a value (kept in this chart's Secret
under `key`) or in an existing Secret. Call with
(dict "root" $ "setting" .Values.registration "key" "registration").
*/}}
{{- define "magnetite-sync.secretKeyRef" -}}
{{- if .setting.existingSecret.name -}}
name: {{ .setting.existingSecret.name }}
key: {{ .setting.existingSecret.key }}
{{- else -}}
name: {{ include "magnetite-sync.fullname" .root }}
key: {{ .key }}
{{- end -}}
{{- end -}}
