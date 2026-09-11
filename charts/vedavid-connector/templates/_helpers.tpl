{{- define "connector.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "connector.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "connector.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "connector.labels" -}}
app.kubernetes.io/name: {{ include "connector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "connector.selectorLabels" -}}
app.kubernetes.io/name: {{ include "connector.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* The host of relay.address, unless a name is given explicitly. */}}
{{- define "connector.serverName" -}}
{{- if .Values.relay.serverName -}}
{{- .Values.relay.serverName -}}
{{- else -}}
{{- (splitList ":" .Values.relay.address) | first -}}
{{- end -}}
{{- end -}}
